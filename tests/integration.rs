//! End-to-end tests against a fake in-process Minecraft server.

use std::net::SocketAddr;
use std::time::Duration;

use lite_mc_ping::{Error, PingOptions, ServerAddress, ping};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// What the fake server should do after the handshake.
#[derive(Clone, Copy)]
enum ServerBehavior {
    /// Reply with status JSON, then close.
    StatusOnly,
    /// Reply with status JSON, then answer one ping/pong exchange.
    WithPong,
    /// Delay (simulating a hung server).
    Delay(Duration),
}

struct FakeServer {
    addr: SocketAddr,
    handle: tokio::task::JoinHandle<()>,
    /// Host string received in the handshake.
    received_host: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl FakeServer {
    async fn spawn(behavior: ServerBehavior) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received_host = std::sync::Arc::new(std::sync::Mutex::new(None));
        let host_capture = received_host.clone();
        let host_capture_task = host_capture.clone();

        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Handshake frame
            let body = read_frame(&mut stream).await.unwrap();
            let mut cursor = std::io::Cursor::new(&body);
            let _packet_id = read_cache_varint(&mut cursor).unwrap();
            let _protocol = read_cache_varint(&mut cursor).unwrap();
            let host_len = read_cache_varint(&mut cursor).unwrap() as usize;
            let mut host = vec![0u8; host_len];
            std::io::Read::read_exact(&mut cursor, &mut host).unwrap();
            *host_capture_task.lock().unwrap() = Some(String::from_utf8(host).unwrap());
            // (Port and next-state follow; not checked in tests.)

            match behavior {
                ServerBehavior::Delay(d) => {
                    tokio::time::sleep(d).await;
                    let _ = stream.shutdown().await;
                }
                _ => {
                    // Consume the status request (frame body [0x00]) before replying.
                    let _status_request = read_frame(&mut stream).await.unwrap();
                    send_status(&mut stream).await.unwrap();
                    if matches!(behavior, ServerBehavior::WithPong) {
                        let ping = read_frame(&mut stream).await.unwrap();
                        assert!(ping.first() == Some(&0x01) && ping.len() == 9);
                        // Build a proper framed pong: id 0x01 + echoed timestamp.
                        let mut pong = Vec::with_capacity(10);
                        pong.push(9); // body length: id + 8 bytes
                        pong.extend_from_slice(&ping);
                        stream.write_all(&pong).await.unwrap();
                        stream.flush().await.unwrap();
                    }
                }
            }
        });

        Self {
            addr,
            handle,
            received_host: host_capture,
        }
    }

    async fn finish(self) {
        self.handle.await.unwrap();
    }

    fn handshake_host(&self) -> String {
        self.received_host
            .lock()
            .unwrap()
            .clone()
            .expect("no handshake captured")
    }
}

fn server_address(addr: SocketAddr) -> ServerAddress {
    // IP literal → SRV skipped, deterministic in tests.
    ServerAddress::new(addr.ip().to_string(), addr.port())
}

// ─── Fake-server wire helpers (mirror the protocol minimally) ────────────────

async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> std::io::Result<Vec<u8>> {
    let len = read_len(reader).await? as usize;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(buf)
}

async fn read_len<R: AsyncRead + Unpin>(reader: &mut R) -> std::io::Result<u32> {
    let mut value: u32 = 0;
    for i in 0..5u32 {
        let byte = reader.read_u8().await?;
        value |= u32::from(byte & 0x7F) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "varint too long",
    ))
}

fn read_cache_varint<R: std::io::Read>(reader: &mut R) -> std::io::Result<u32> {
    let mut value: u32 = 0;
    for i in 0..5u32 {
        let mut b = [0u8; 1];
        reader.read_exact(&mut b)?;
        let byte = b[0];
        value |= u32::from(byte & 0x7F) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "varint too long",
    ))
}

fn status_json() -> Vec<u8> {
    br#"{"version":{"name":"1.21.4","protocol":769},"players":{"max":100,"online":3,"sample":[{"name":"Alice","id":"00000000-0000-0000-0000-000000000001"}]},"description":{"text":"A test server"},"favicon":"data:image/png;base64,AAAA","enforcesSecureChat":true}"#
        .to_vec()
}

async fn send_status(stream: &mut TcpStream) -> std::io::Result<()> {
    let json = status_json();
    let mut body = Vec::with_capacity(json.len() + 8);
    body.push(0x00); // status response packet id
    push_varint(&mut body, json.len() as u32);
    body.extend_from_slice(&json);
    let mut frame = Vec::with_capacity(body.len() + 3);
    push_varint(&mut frame, body.len() as u32);
    frame.extend_from_slice(&body);
    stream.write_all(&frame).await
}

fn push_varint(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn status_only_returns_parsed_fields() {
    let server = FakeServer::spawn(ServerBehavior::StatusOnly).await;
    let result = ping(&server_address(server.addr), &PingOptions::default())
        .await
        .unwrap();

    let s = &result.status;
    assert_eq!(s.version.name, "1.21.4");
    assert_eq!(s.version.protocol, 769);
    assert_eq!(s.players.max, 100);
    assert_eq!(s.players.online, 3);
    assert_eq!(s.players.sample.len(), 1);
    assert_eq!(s.players.sample[0].name, "Alice");
    assert_eq!(s.description, serde_json::json!({"text": "A test server"}));
    assert_eq!(s.favicon.as_deref(), Some("data:image/png;base64,AAAA"));
    assert_eq!(s.enforces_secure_chat, Some(true));
    assert!(result.latency.is_none());

    // Handshake carried the hostname the caller typed.
    assert_eq!(server.handshake_host(), server.addr.ip().to_string());
    server.finish().await;
}

#[tokio::test]
async fn measure_latency_returns_rtt() {
    let server = FakeServer::spawn(ServerBehavior::WithPong).await;
    let options = PingOptions {
        measure_latency: true,
        ..Default::default()
    };
    let result = ping(&server_address(server.addr), &options).await.unwrap();
    assert!(result.latency.is_some());
    server.finish().await;
}

#[tokio::test]
async fn no_latency_measurement_when_disabled() {
    let server = FakeServer::spawn(ServerBehavior::StatusOnly).await;
    let result = ping(&server_address(server.addr), &PingOptions::default())
        .await
        .unwrap();
    assert!(result.latency.is_none());
    server.finish().await;
}

#[tokio::test]
async fn timeout_when_server_is_slow() {
    let server = FakeServer::spawn(ServerBehavior::Delay(Duration::from_millis(300))).await;
    let options = PingOptions {
        timeout: Duration::from_millis(50),
        ..Default::default()
    };
    let err = ping(&server_address(server.addr), &options)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Timeout(_)), "got {err:?}");
    server.finish().await;
}

#[tokio::test]
async fn frame_size_limit_is_enforced() {
    // Fake server frame is ~300 bytes; a tiny limit must reject it.
    let server = FakeServer::spawn(ServerBehavior::StatusOnly).await;
    let options = PingOptions {
        max_frame_size: 64,
        ..Default::default()
    };
    let err = ping(&server_address(server.addr), &options)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::FrameTooLarge { .. }), "got {err:?}");
    server.finish().await;
}

#[tokio::test]
async fn description_as_plain_string_is_accepted() {
    // A server variant that sends the description as a bare string.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = read_frame(&mut stream).await.unwrap();
        let json = br#"{"version":{"name":"1.8.9","protocol":47},"players":{"max":10,"online":0},"description":"Plain motd"}"#;
        let mut body = Vec::new();
        body.push(0x00);
        push_varint(&mut body, json.len() as u32);
        body.extend_from_slice(json);
        let mut frame = Vec::with_capacity(body.len() + 3);
        push_varint(&mut frame, body.len() as u32);
        frame.extend_from_slice(&body);
        stream.write_all(&frame).await.unwrap();
    });

    let result = ping(&server_address(addr), &PingOptions::default())
        .await
        .unwrap();
    assert_eq!(result.status.description, serde_json::json!("Plain motd"));
    // Missing optional fields defaulted.
    assert_eq!(result.status.favicon, None);
    assert_eq!(result.status.enforces_secure_chat, None);
    handle.await.unwrap();
}
