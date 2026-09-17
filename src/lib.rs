//! Minimal async Minecraft **Java Edition** (1.7+, wire protocol 47+) server
//! list ping.
//!
//! Implements the modern protocol over TCP:
//!
//! ```text
//! Client → Server: Handshake    (0x00) protocol, host, port, next_state=1
//! Client → Server: Status req   (0x00)
//! Server → Client: Status resp  (0x00) JSON: version / players / description…
//! Client → Server: Ping         (0x01) i64 timestamp     [optional]
//! Server → Client: Pong         (0x01) same timestamp    [optional]
//! ```
//!
//! # Features
//!
//! * **SRV resolution** — `_minecraft._tcp.<host>` via `hickory-resolver`,
//!   with RFC 2782 weighted-random target selection. On lookup failure (or for
//!   IP literals) it falls back to the direct address. When an SRV record is
//!   used, the handshake still carries the *original* hostname, as Mojang's
//!   client does, so servers can virtual-host on it.
//! * **Optional latency** — set [`PingOptions::measure_latency`] to perform
//!   the extra ping/pong round trip; the RTT is returned in
//!   [`PingResult::latency`].
//! * **Async API** on tokio.
//!
//! # Example
//!
//! ```no_run
//! use lite_mc_ping::{ping, ServerAddress, PingOptions};
//!
//! #[tokio::main]
//! async fn main() {
//!     let address: ServerAddress = "play.example.com:25565".parse().unwrap();
//!     let options = PingOptions {
//!         measure_latency: true,
//!         ..Default::default()
//!     };
//!     let result = ping(&address, &options).await.unwrap();
//!     println!("{} players online (max {})",
//!         result.status.players.online, result.status.players.max);
//!     if let Some(latency) = result.latency {
//!         println!("latency: {latency:?}");
//!     }
//! }
//! ```
//!
//! # Design notes & trade-offs (vs. `rust-mc-status`)
//!
//! * No proxying, no legacy (≤1.6) ping, no Forge/mod parsing; scope is the
//!   core status flow.
//! * The [`varint`] crate is used for VarInt codec, with only its *unsigned*
//!   methods: the signed methods are protobuf-zigzag, which does not match
//!   Minecraft's two's-complement VarInt (protocol `-1` encodes as
//!   `FF FF FF FF 0F`, via `i32 as u32`). The frame-length prefix is the only
//!   VarInt read incrementally from the async stream and is handled by a
//!   5-byte-capped loop, as the crate is `std::io`-only; all other VarInts
//!   decode through the crate.
//! * Status JSON deserializes into typed structs ([`StatusResponse`]),
//!   leniently: unknown fields are ignored and optional fields default.
//!   `description` remains `serde_json::Value` — it is either a plain string
//!   or a Chat-component object.

mod error;
mod models;
mod protocol;
mod srv;

use std::net::IpAddr;
use std::time::Instant;

use tokio::io::{AsyncWriteExt, BufStream};
use tokio::net::TcpStream;
use tokio::time::timeout;

pub use error::Error;
pub use models::{
    DEFAULT_PORT, PingOptions, PingResult, PlayerSample, Players, ResolvedAddress, ServerAddress,
    StatusResponse, Version,
};

/// Resolve the effective address to connect to, applying SRV lookup when
/// applicable (host is not an IP literal and [`PingOptions::use_srv`]).
///
/// The result is also used internally by [`ping`]; the port may come from the
/// SRV record while the handshake keeps the original hostname.
pub async fn resolve_server_address(host: &str, port: u16) -> Result<ResolvedAddress, Error> {
    resolve_server_address_with_options(host, port, &PingOptions::default()).await
}

/// Same as [`resolve_server_address`] but honoring the caller's
/// [`PingOptions::use_srv`] flag.
pub async fn resolve_server_address_with_options(
    host: &str,
    port: u16,
    options: &PingOptions,
) -> Result<ResolvedAddress, Error> {
    if !options.use_srv || host.parse::<IpAddr>().is_ok() {
        return Ok(ResolvedAddress {
            host: host.to_string(),
            port,
            used_srv: false,
        });
    }

    // Reuse the process-wide resolver (shared DNS cache); on init failure or
    // when there is no usable SRV record, connect directly.
    if let Some(resolver) = srv::shared_resolver()
        && let Some(record) = srv::resolve_srv(resolver, host, &mut *srv::rng()).await?
    {
        return Ok(ResolvedAddress {
            host: record.target,
            port: record.port,
            used_srv: true,
        });
    }

    Ok(ResolvedAddress {
        host: host.to_string(),
        port,
        used_srv: false,
    })
}

/// Ping a Minecraft Java server and (optionally) measure latency.
///
/// The whole operation is bounded by [`PingOptions::timeout`].
pub async fn ping(address: &ServerAddress, options: &PingOptions) -> Result<PingResult, Error> {
    let resolved =
        resolve_server_address_with_options(&address.host, address.port, options).await?;
    timeout(
        options.timeout,
        ping_inner(&resolved, &address.host, options),
    )
    .await
    .map_err(|_| Error::Timeout(options.timeout))?
}

async fn ping_inner(
    resolved: &ResolvedAddress,
    original_host: &str,
    options: &PingOptions,
) -> Result<PingResult, Error> {
    // Wildcard SRV targets ("*.example.com") cannot be resolved to a host.
    if resolved.host.contains('*') {
        return Err(Error::Dns(format!(
            "SRV target {:?} is a wildcard and cannot be connected to",
            resolved.host
        )));
    }
    // Connecting by hostname lets tokio resolve A/AAAA via the OS and try
    // every address in order, giving multi-address targets automatic failover.
    let mut stream =
        BufStream::new(TcpStream::connect((resolved.host.as_str(), resolved.port)).await?);
    stream.get_ref().set_nodelay(true)?;

    // Handshake: carry the *original* hostname, connect to the SRV port.
    stream
        .write_all(&protocol::build_handshake(
            options.protocol_version,
            original_host,
            resolved.port,
        )?)
        .await?;
    stream.flush().await?;

    stream.write_all(&protocol::build_status_request()?).await?;
    stream.flush().await?;

    let status_frame = protocol::read_frame(&mut stream, options.max_frame_size).await?;
    let status = protocol::parse_status_frame(&status_frame)?;

    let mut latency = None;
    if options.measure_latency {
        stream
            .write_all(&protocol::build_ping_packet(protocol::now_nanos())?)
            .await?;
        stream.flush().await?;
        let start = Instant::now();
        let pong_frame = protocol::read_frame(&mut stream, options.max_frame_size).await?;
        protocol::parse_pong_frame(&pong_frame)?;
        latency = Some(start.elapsed());
    }

    Ok(PingResult { status, latency })
}
