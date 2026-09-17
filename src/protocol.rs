//! Wire protocol encode/decode for the modern (1.7+) server list ping.
//!
//! Packet structure (see <https://wiki.vg/Server_List_Ping>):
//!
//! ```text
//! Client → Server: Handshake   (0x00) protocol_version, host, port, state=1
//! Client → Server: Status req  (0x00, empty)
//! Server → Client: Status resp (0x00) JSON string
//! Client → Server: Ping        (0x01) i64 timestamp (big-endian)
//! Server → Client: Pong        (0x01) same i64
//! ```
//!
//! Every packet is prefixed with a VarInt frame length. One design note on the
//! [`varint`] crate: its `read/write_signed_varint_32` methods use protobuf
//! *zigzag* encoding, which does **not** match Minecraft's VarInt — Minecraft
//! uses a plain unsigned varint of the two's-complement value (protocol
//! version `-1` → bytes `FF FF FF FF 0F`). We therefore only use the crate's
//! unsigned methods and cast `i32 → u32` ourselves. Also, the crate works over
//! `std::io` only, so the single VarInt read incrementally from the async
//! stream (the frame-length prefix) is handled by a small 5-byte-capped loop;
//! every other VarInt is decoded through the crate.

use std::io::{Cursor, Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncRead, AsyncReadExt};
use varint::{VARINT_32_MAX_BYTES, VarintRead, VarintWrite};

use crate::error::Error;
use crate::models::StatusResponse;

/// Max bytes an in-memory u32 VarInt can occupy.
const MAX_VARINT_BYTES: u64 = VARINT_32_MAX_BYTES as u64;

// ─── Encoding ────────────────────────────────────────────────────────────────

/// Build the handshake packet, already framed (length prefix included).
///
/// `host` is the address the *user* typed — after SRV resolution the target
/// may differ, but the handshake must still carry the original hostname so the
/// server can do virtual hosting.
pub(crate) fn build_handshake(protocol: i32, host: &str, port: u16) -> Result<Vec<u8>, Error> {
    let mut body = Cursor::new(Vec::with_capacity(1 + 5 + 1 + host.len() + 2 + 1));
    body.write_unsigned_varint_32(0x00)?; // packet id: handshake
    body.write_unsigned_varint_32(protocol as u32)?; // protocol version, two's complement
    body.write_unsigned_varint_32(host.len() as u32)?; // string length prefix
    body.write_all(host.as_bytes())?;
    body.write_all(&port.to_be_bytes())?;
    body.write_unsigned_varint_32(1)?; // next state: status
    pack_frame(body.into_inner())
}

/// Build the status request packet: `0x00` with empty body.
pub(crate) fn build_status_request() -> Result<Vec<u8>, Error> {
    pack_frame(vec![0x00])
}

/// Build the ping packet: `0x01` followed by the i64 timestamp (big-endian).
pub(crate) fn build_ping_packet(timestamp: i64) -> Result<Vec<u8>, Error> {
    let mut body = Vec::with_capacity(1 + 8);
    body.push(0x01);
    body.extend_from_slice(&timestamp.to_be_bytes());
    pack_frame(body)
}

fn pack_frame(body: Vec<u8>) -> Result<Vec<u8>, Error> {
    let mut frame = Cursor::new(Vec::with_capacity(body.len() + 5));
    frame.write_unsigned_varint_32(body.len() as u32)?;
    frame.write_all(&body)?;
    Ok(frame.into_inner())
}

// ─── Decoding ────────────────────────────────────────────────────────────────

/// Read the frame-length VarInt from an async stream, capped at 5 bytes.
///
/// This is the only wire VarInt not read through the [`varint`] crate, because
/// the crate is synchronous (`std::io` only) while the stream is tokio-based.
pub(crate) async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_len: u32,
) -> Result<Vec<u8>, Error> {
    let mut value: u32 = 0;
    for i in 0..5u32 {
        let byte = reader.read_u8().await?;
        value |= u32::from(byte & 0x7F) << (7 * i);
        if byte & 0x80 == 0 {
            return read_frame_body(reader, max_len, value).await;
        }
    }
    Err(Error::Malformed(
        "frame length VarInt exceeds 5 bytes".into(),
    ))
}

async fn read_frame_body<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_len: u32,
    len: u32,
) -> Result<Vec<u8>, Error> {
    if len > max_len {
        return Err(Error::FrameTooLarge {
            len,
            limit: max_len,
        });
    }
    let mut frame = vec![0u8; len as usize];
    reader.read_exact(&mut frame).await?;
    Ok(frame)
}

/// Parse a status response frame into typed JSON.
pub(crate) fn parse_status_frame(frame: &[u8]) -> Result<StatusResponse, Error> {
    let mut cur = Cursor::new(frame);
    let id = read_varint(&mut cur)?;
    if id != 0x00 {
        return Err(Error::UnexpectedPacket {
            expected: 0x00,
            got: id as u8,
        });
    }
    let json_len = read_varint(&mut cur)? as usize;
    let start = cur.position() as usize;
    let end = start
        .checked_add(json_len)
        .filter(|&e| e <= frame.len())
        .ok_or_else(|| {
            Error::Malformed(format!(
                "JSON length overruns frame: {start} + {json_len} > {}",
                frame.len()
            ))
        })?;
    serde_json::from_slice(&frame[start..end]).map_err(Into::into)
}

/// Parse a pong frame, returning the echoed i64 timestamp.
pub(crate) fn parse_pong_frame(frame: &[u8]) -> Result<i64, Error> {
    let mut cur = Cursor::new(frame);
    let id = read_varint(&mut cur)?;
    if id != 0x01 {
        return Err(Error::UnexpectedPacket {
            expected: 0x01,
            got: id as u8,
        });
    }
    let payload = &frame[cur.position() as usize..];
    if payload.len() != 8 {
        return Err(Error::Malformed(format!(
            "pong payload must be 8 bytes, got {}",
            payload.len()
        )));
    }
    Ok(i64::from_be_bytes(payload.try_into().expect("len checked")))
}

/// Read a VarInt from an in-memory reader via the [`varint`] crate.
///
/// The crate only implements `VarintRead` for `Cursor<Vec<u8>>`, and its loop
/// has no 5-byte cap (a 6th continuation byte would shift past 32 bits), so we
/// first copy at most [`MAX_VARINT_BYTES`] bytes from the reader into a small
/// window and decode that window with the crate. This never over-reads beyond
/// the VarInt and never panics on hostile input.
fn read_varint<R: Read>(reader: &mut R) -> Result<u32, Error> {
    let mut window = Vec::with_capacity(MAX_VARINT_BYTES as usize);
    let mut take = reader.by_ref().take(MAX_VARINT_BYTES);
    let mut byte = [0u8; 1];
    loop {
        match take.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => window.push(byte[0]),
            Err(_) => break,
        }
        if byte[0] & 0x80 == 0 {
            break;
        }
    }
    Cursor::new(window)
        .read_unsigned_varint_32()
        .map_err(|e| Error::Malformed(format!("bad VarInt: {e}")))
}

/// Current wall-clock time in nanoseconds, as the opaque i64 payload of the
/// ping/pong exchange (the protocol does not interpret it).
pub(crate) fn now_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn handshake_bytes_match_known_good() {
        // protocol -1 (latest) must encode as five 0xFF bytes, not zigzag!
        let frame = build_handshake(-1, "localhost", 25565).unwrap();
        let expected: &[u8] = &[
            0x13, // frame length (19)
            0x00, // packet id
            0xFF, 0xFF, 0xFF, 0xFF, 0x0F, // protocol version -1
            0x09, // "localhost"
            b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't', 0x63, 0xDD, // port 25565
            0x01, // next state: status
        ];
        assert_eq!(frame, expected);
    }

    #[test]
    fn handshake_with_positive_protocol() {
        let frame = build_handshake(765, "mc.example.com", 25566).unwrap();
        let expected: &[u8] = &[
            0x15, // 21
            0x00, // packet id
            0xFD, 0x05, // 765 varint
            0x0E, // "mc.example.com" (14)
            b'm', b'c', b'.', b'e', b'x', b'a', b'm', b'p', b'l', b'e', b'.', b'c', b'o', b'm',
            0x63, 0xDE, // 25566
            0x01,
        ];
        assert_eq!(frame, expected);
    }

    #[test]
    fn status_request_frame() {
        assert_eq!(build_status_request().unwrap(), vec![0x01, 0x00]);
    }

    #[test]
    fn ping_packet_frame() {
        let frame = build_ping_packet(0x0102030405060708).unwrap();
        assert_eq!(
            frame,
            vec![0x09, 0x01, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]
        );
    }

    #[test]
    fn varint_roundtrip_via_crate() {
        for value in [
            0u32,
            1,
            127,
            128,
            255,
            300,
            16_383,
            16_384,
            0x7FFF_FFFF,
            0xFFFF_FFFF,
        ] {
            let mut out = Cursor::new(Vec::new());
            out.write_unsigned_varint_32(value).unwrap();
            let mut inp = Cursor::new(out.into_inner());
            assert_eq!(read_varint(&mut inp).unwrap(), value, "value {value}");
        }
    }

    #[test]
    fn varint_rejects_sixth_continuation_byte() {
        // 5 continuation bytes + one more: would exceed u32 and panic in the
        // crate's unbounded loop if not capped.
        let data: &[u8] = &[0x80, 0x80, 0x80, 0x80, 0x80, 0x01];
        let mut cur = Cursor::new(data);
        assert!(matches!(read_varint(&mut cur), Err(Error::Malformed(_))));
    }

    #[test]
    fn parse_status_frame_ok() {
        let json = br#"{"version":{"name":"1.21.4","protocol":769},"players":{"max":100,"online":2},"description":{"text":"Hello"}}"#;
        let mut body = Cursor::new(Vec::new());
        body.write_unsigned_varint_32(0x00).unwrap();
        body.write_unsigned_varint_32(json.len() as u32).unwrap();
        std::io::Write::write_all(&mut body, json).unwrap();

        let status = parse_status_frame(&body.into_inner()).unwrap();
        assert_eq!(status.version.name, "1.21.4");
        assert_eq!(status.version.protocol, 769);
        assert_eq!(status.players.max, 100);
        assert_eq!(status.players.online, 2);
        assert_eq!(status.description, serde_json::json!({"text": "Hello"}));
    }

    #[test]
    fn parse_status_frame_wrong_packet_id() {
        let mut body = Cursor::new(Vec::new());
        body.write_unsigned_varint_32(0x02).unwrap();
        assert!(matches!(
            parse_status_frame(&body.into_inner()),
            Err(Error::UnexpectedPacket {
                expected: 0x00,
                got: 0x02
            })
        ));
    }

    #[test]
    fn parse_status_frame_json_overruns_frame() {
        let json = b"{}";
        let mut body = Cursor::new(Vec::new());
        body.write_unsigned_varint_32(0x00).unwrap();
        body.write_unsigned_varint_32(100_000).unwrap(); // claims more than present
        std::io::Write::write_all(&mut body, json).unwrap();
        assert!(matches!(
            parse_status_frame(&body.into_inner()),
            Err(Error::Malformed(_))
        ));
    }

    #[test]
    fn parse_pong_ok() {
        let mut body = Cursor::new(Vec::new());
        body.write_unsigned_varint_32(0x01).unwrap();
        std::io::Write::write_all(&mut body, &0x1122334455667788i64.to_be_bytes()).unwrap();
        assert_eq!(
            parse_pong_frame(&body.into_inner()).unwrap(),
            0x1122334455667788
        );
    }

    #[tokio::test]
    async fn read_frame_caps_varint_at_five_bytes() {
        let (mut rx, mut tx) = tokio::io::duplex(64);
        tx.write_all(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x01])
            .await
            .unwrap();
        let err = read_frame(&mut rx, 1024).await.unwrap_err();
        assert!(matches!(err, Error::Malformed(_)));
    }

    #[tokio::test]
    async fn read_frame_enforces_max_size() {
        let (mut rx, mut tx) = tokio::io::duplex(64);
        tx.write_all(&[0x0A]).await.unwrap(); // declared length 10
        tx.write_all(&[0u8; 10]).await.unwrap();
        let err = read_frame(&mut rx, 8).await.unwrap_err();
        assert!(matches!(err, Error::FrameTooLarge { .. }));
    }

    #[tokio::test]
    async fn read_frame_reads_exact_body() {
        let (mut rx, mut tx) = tokio::io::duplex(64);
        let payload = [0x00, 0x01, 0x02, 0x03];
        tx.write_all(&[payload.len() as u8]).await.unwrap();
        tx.write_all(&payload).await.unwrap();
        let frame = read_frame(&mut rx, 1024).await.unwrap();
        assert_eq!(frame, payload);
    }
}
