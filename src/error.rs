//! Error type for the crate.

use std::time::Duration;

use thiserror::Error;

/// All errors that can occur while pinging a Minecraft server.
#[derive(Debug, Error)]
pub enum Error {
    /// Underlying TCP / I/O failure (connect, read, write, ...).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// DNS resolution via hickory-resolver failed.
    #[error("DNS resolution error: {0}")]
    Dns(String),

    /// The whole ping operation exceeded [`crate::PingOptions::timeout`].
    #[error("timed out after {0:?}")]
    Timeout(Duration),

    /// The server sent bytes that do not form a valid protocol frame.
    #[error("malformed response: {0}")]
    Malformed(String),

    /// The server replied with a packet id we did not ask for.
    #[error("unexpected packet: expected {expected:#04x}, got {got:#04x}")]
    UnexpectedPacket { expected: u8, got: u8 },

    /// The incoming frame exceeds the configured [`crate::PingOptions::max_frame_size`].
    #[error("frame too large: {len} bytes (limit {limit})")]
    FrameTooLarge { len: u32, limit: u32 },

    /// The status JSON payload could not be parsed.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}
