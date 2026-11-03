//! Wire-level tests for the status protocol, run against a fake server that
//! speaks the bytes a real Minecraft server does: it checks the client's
//! frames byte for byte and answers with hand-built frames, so nothing here
//! leans on the crate's own encoders or on the network.

use std::future::Future;
use std::time::Duration;

use lite_mc_ping::{Error, PingOptions, ServerAddress, ping};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// Status JSON shaped like a modern server's: the MOTD is a Chat component.
const STATUS_JSON: &str = r#"{"version":{"name":"1.21.4","protocol":769},"players":{"max":100,"online":2},"description":{"text":"Hi","color":"gold","extra":[{"text":" there","bold":true}]}}"#;

fn options() -> PingOptions {
    PingOptions {
        timeout: Duration::from_secs(5),
        ..Default::default()
    }
}

/// Accept one connection on a fresh loopback port and run the handler on it.
///
/// The port is already bound when this returns, so the client can connect.
async fn serve<F, Fut>(handler: F) -> (u16, JoinHandle<()>)
where
    F: FnOnce(TcpStream) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        handler(socket).await;
    });
    (port, server)
}

/// VarInt as Minecraft encodes it: seven bits per byte, little endian groups.
fn varint_bytes(mut value: u32) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            return out;
        }
    }
}

/// A packet as it goes on the wire: VarInt length, then the payload.
fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = varint_bytes(payload.len() as u32);
    out.extend_from_slice(payload);
    out
}

/// Status response: packet id, then the JSON string.
fn status_frame(id: u8, json: &str) -> Vec<u8> {
    let mut payload = vec![id];
    payload.extend(varint_bytes(json.len() as u32));
    payload.extend_from_slice(json.as_bytes());
    frame(&payload)
}

/// Pong: packet id, then the timestamp the ping carried.
fn pong_frame(timestamp: &[u8]) -> Vec<u8> {
    let mut payload = vec![0x01];
    payload.extend_from_slice(timestamp);
    frame(&payload)
}

async fn read_varint(socket: &mut TcpStream) -> i32 {
    let mut value = 0u32;
    for shift in 0..5 {
        let byte = socket.read_u8().await.expect("read varint");
        value |= u32::from(byte & 0x7F) << (7 * shift);
        if byte & 0x80 == 0 {
            return value as i32;
        }
    }
    panic!("VarInt longer than 5 bytes");
}

/// Read one frame from the client, length prefix included.
async fn read_frame(socket: &mut TcpStream) -> Vec<u8> {
    let len = read_varint(socket).await as usize;
    let mut payload = vec![0u8; len];
    socket.read_exact(&mut payload).await.expect("read payload");
    payload
}

/// Decode a VarInt from the front of a received payload.
fn take_varint(bytes: &mut &[u8]) -> i32 {
    let mut value = 0u32;
    for shift in 0..5 {
        let (&byte, rest) = bytes.split_first().expect("truncated varint");
        *bytes = rest;
        value |= u32::from(byte & 0x7F) << (7 * shift);
        if byte & 0x80 == 0 {
            return value as i32;
        }
    }
    panic!("VarInt longer than 5 bytes");
}

/// The client's frames must be exactly what a vanilla server expects:
/// handshake 0x00 with a two's-complement protocol version, the host string
/// the caller asked for, the connected port, and next state 1 — then an empty
/// 0x00 status request.
#[tokio::test]
async fn client_sends_the_frames_a_real_server_expects() {
    let (port, server) = serve(|mut socket| async move {
        let handshake = read_frame(&mut socket).await;
        assert_eq!(handshake[0], 0x00, "packet id: handshake");
        assert_eq!(
            &handshake[1..6],
            &[0xFF, 0xFF, 0xFF, 0xFF, 0x0F][..],
            "protocol -1 must be two's complement, not zigzag"
        );

        let mut rest = &handshake[6..];
        let host_len = take_varint(&mut rest) as usize;
        assert_eq!(&rest[..host_len], b"127.0.0.1");
        rest = &rest[host_len..];
        let port = u16::from_be_bytes([rest[0], rest[1]]);
        assert_eq!(port, socket.local_addr().unwrap().port());
        rest = &rest[2..];
        assert_eq!(take_varint(&mut rest), 1, "next state: status");
        assert!(rest.is_empty(), "nothing follows the next state");

        assert_eq!(read_frame(&mut socket).await, vec![0x00], "status request");

        socket
            .write_all(&status_frame(0x00, STATUS_JSON))
            .await
            .unwrap();
    })
    .await;

    let result = ping(&ServerAddress::new("127.0.0.1", port), &options())
        .await
        .unwrap();
    assert_eq!(result.status.version.name, "1.21.4");
    server.await.unwrap();
}

/// TCP may split a frame across segments; a real server's answer must still be
/// reassembled. The MOTD is a component, so it also checks the legacy text.
#[tokio::test]
async fn reassembles_a_status_frame_split_across_segments() {
    let (port, server) = serve(|mut socket| async move {
        read_frame(&mut socket).await; // handshake
        read_frame(&mut socket).await; // status request
        for byte in status_frame(0x00, STATUS_JSON) {
            socket.write_all(&[byte]).await.unwrap();
            tokio::task::yield_now().await;
        }
    })
    .await;

    let status = ping(&ServerAddress::new("127.0.0.1", port), &options())
        .await
        .unwrap()
        .status;
    assert_eq!(status.version.protocol, 769);
    assert_eq!(status.players.online, 2);
    assert_eq!(status.description, "§6Hi§l there");
    server.await.unwrap();
}

/// With latency measurement on, the client sends 0x01 carrying one i64 and
/// takes the echoed pong as its round-trip time.
#[tokio::test]
async fn measures_latency_from_the_echoed_pong() {
    let (port, server) = serve(|mut socket| async move {
        read_frame(&mut socket).await; // handshake
        read_frame(&mut socket).await; // status request
        socket
            .write_all(&status_frame(0x00, STATUS_JSON))
            .await
            .unwrap();

        let ping = read_frame(&mut socket).await;
        assert_eq!(ping[0], 0x01, "packet id: ping");
        assert_eq!(ping.len(), 9, "ping carries exactly one i64");

        socket.write_all(&pong_frame(&ping[1..])).await.unwrap();
    })
    .await;

    let opts = PingOptions {
        measure_latency: true,
        ..options()
    };
    let result = ping(&ServerAddress::new("127.0.0.1", port), &opts)
        .await
        .unwrap();
    assert_eq!(result.status.version.protocol, 769);
    assert!(result.latency.is_some(), "an answered ping is measured");
    server.await.unwrap();
}

#[tokio::test]
async fn rejects_a_status_response_with_the_wrong_packet_id() {
    let (port, server) = serve(|mut socket| async move {
        read_frame(&mut socket).await; // handshake
        read_frame(&mut socket).await; // status request
        socket
            .write_all(&status_frame(0x02, STATUS_JSON))
            .await
            .unwrap();
    })
    .await;

    let err = ping(&ServerAddress::new("127.0.0.1", port), &options())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::UnexpectedPacket {
                expected: 0x00,
                got: 0x02
            }
        ),
        "{err:?}"
    );
    server.await.unwrap();
}

/// A declared length is a promise; one beyond the configured max_frame_size is
/// refused before any of the payload is read.
#[tokio::test]
async fn rejects_a_frame_larger_than_the_configured_limit() {
    let (port, server) = serve(|mut socket| async move {
        read_frame(&mut socket).await; // handshake
        read_frame(&mut socket).await; // status request
        socket.write_all(&varint_bytes(8192)).await.unwrap();
        socket.write_all(&[0x00; 32]).await.unwrap();
    })
    .await;

    let opts = PingOptions {
        max_frame_size: 1024,
        ..options()
    };
    let err = ping(&ServerAddress::new("127.0.0.1", port), &opts)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::FrameTooLarge {
                len: 8192,
                limit: 1024
            }
        ),
        "{err:?}"
    );
    server.await.unwrap();
}

/// Older servers send the MOTD as a plain string and omit every optional
/// field; that status must parse all the same.
#[tokio::test]
async fn parses_a_plain_string_motd_with_optional_fields_missing() {
    const JSON: &str = r#"{"version":{"name":"1.8.9","protocol":47},"players":{"max":20,"online":1},"description":"Welcome!"}"#;
    let (port, server) = serve(|mut socket| async move {
        read_frame(&mut socket).await; // handshake
        read_frame(&mut socket).await; // status request
        socket.write_all(&status_frame(0x00, JSON)).await.unwrap();
    })
    .await;

    let status = ping(&ServerAddress::new("127.0.0.1", port), &options())
        .await
        .unwrap()
        .status;
    assert_eq!(status.version.protocol, 47);
    assert_eq!(status.description, "Welcome!");
    assert_eq!(status.favicon, None);
    assert_eq!(status.enforces_secure_chat, None);
    assert_eq!(status.previews_chat, None);
    server.await.unwrap();
}
