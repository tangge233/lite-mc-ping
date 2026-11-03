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
//! Every packet is prefixed with a VarInt frame length. Note on the [`varint`]
//! crate: its `read/write_signed_varint_32` methods use protobuf *zigzag*
//! encoding, which does **not** match Minecraft's VarInt — Minecraft encodes
//! the two's-complement value as a plain unsigned varint (protocol `-1` →
//! bytes `FF FF FF FF 0F`). Only the unsigned methods are used, casting
//! `i32 → u32`. As the crate is `std::io`-only, the frame-length prefix (the
//! only VarInt read incrementally from the async stream) uses a 5-byte-capped
//! loop; every other VarInt decodes through the crate.

use std::io::Cursor;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use varint_rs::{VarintReader, VarintWriter};

use crate::ResolvedAddress;
use crate::error::Error;
use crate::models::StatusResponse;

const MAX_VARINT_BYTES: usize = 5;

pub struct McProtocol<S> {
    inner_stream: S,
    frame_max_size: usize,
}

impl<S> McProtocol<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub fn new(stream: S, frame_max_size: usize) -> Self {
        Self {
            inner_stream: stream,
            frame_max_size,
        }
    }

    pub async fn write_handshake_frame(
        &mut self,
        protocol: i32,
        addr: &ResolvedAddress,
    ) -> Result<(), Error> {
        let payload_len = 1 + 5 + 1 + addr.host.len() + 2 + 1;
        let mut buf = Cursor::new(Vec::with_capacity(payload_len));
        buf.write_u32_varint(0x00)?; // packet id: handshake
        buf.write_u32_varint(protocol as u32)?; // protocol version, two's complement
        buf.write_u32_varint(addr.host.len() as u32)?; // string length prefix
        buf.write_all(addr.host.as_bytes()).await?;
        buf.write_all(&addr.port.to_be_bytes()).await?;
        buf.write_u32_varint(1)?; // next state: status
        self.write_frame(&buf.into_inner()).await?;
        Ok(())
    }

    pub async fn write_status_request(&mut self) -> Result<(), Error> {
        self.write_frame(&vec![0x00]).await?;
        Ok(())
    }

    pub async fn write_ping_packet(&mut self, timestamp: i64) -> Result<(), Error> {
        let mut payload = Vec::with_capacity(1 + 8);
        payload.push(0x01);
        payload.extend_from_slice(&timestamp.to_be_bytes());
        self.write_frame(&payload).await?;
        Ok(())
    }

    pub async fn read_status_response(&mut self) -> Result<StatusResponse, Error> {
        let frame = self.read_frame().await?;
        let mut cur = Cursor::new(frame);
        let id = cur.read_u32_varint()?;
        if id != 0x00 {
            return Err(Error::UnexpectedPacket {
                expected: 0x00,
                got: u8::try_from(id).unwrap_or(0xFF),
            });
        }
        let json_len = cur.read_u32_varint()? as usize;
        let start = cur.position() as usize;
        let frame = cur.into_inner();
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

    pub async fn read_pong_response(&mut self) -> Result<i64, Error> {
        let frame = self.read_frame().await?;
        let mut cur = Cursor::new(frame);
        let id = cur.read_u32_varint()?;
        if id != 0x01 {
            return Err(Error::UnexpectedPacket {
                expected: 0x01,
                got: u8::try_from(id).unwrap_or(0xFF),
            });
        }
        let start = cur.position() as usize;
        let frame = cur.into_inner();
        let payload = &frame[start..];
        if payload.len() != 8 {
            return Err(Error::Malformed(format!(
                "pong payload must be 8 bytes, got {}",
                payload.len()
            )));
        }
        Ok(i64::from_be_bytes(payload.try_into().expect("len checked")))
    }

    async fn read_frame(&mut self) -> Result<Vec<u8>, Error> {
        // Length prefix, byte at a time: it may split across TCP segments.
        let mut payload_len: u32 = 0;
        for shift in 0..MAX_VARINT_BYTES as u32 {
            let byte = self.inner_stream.read_u8().await?;
            payload_len |= u32::from(byte & 0x7F) << (7 * shift);
            if byte & 0x80 == 0 {
                if payload_len as usize > self.frame_max_size {
                    return Err(Error::FrameTooLarge {
                        len: payload_len as usize,
                        limit: self.frame_max_size,
                    });
                }
                let mut payload = vec![0u8; payload_len as usize];
                self.inner_stream.read_exact(&mut payload).await?;
                return Ok(payload);
            }
        }
        Err(Error::Malformed(
            "frame length VarInt exceeds 5 bytes".into(),
        ))
    }

    async fn write_frame(&mut self, payload: &[u8]) -> Result<(), Error> {
        let mut frame = Cursor::new(vec![0u8; 5]);
        frame.write_u32_varint(payload.len() as u32)?;
        let pos = frame.position() as usize;
        self.inner_stream
            .write_all(&frame.into_inner()[..pos])
            .await?;
        self.inner_stream.write_all(payload).await?;
        self.inner_stream.flush().await?;
        Ok(())
    }
}

/// Current wall-clock time in nanoseconds, as the opaque i64 payload of the
/// ping/pong exchange (the protocol does not interpret it).
pub(crate) fn now_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}
