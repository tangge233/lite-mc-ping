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
//!   with RFC 2782 weighted-random target selection. Looked up only when the
//!   address leaves the port open: an explicit port (`"play.example.com:25566"`)
//!   or an IP literal is used as-is. A failed lookup, or no usable record,
//!   falls back to the address as given. When an SRV record is used, the
//!   handshake still carries the *original* hostname, as Mojang's client does,
//!   so servers can virtual-host on it.
//! * **Optional latency** — set [`PingOptions::measure_latency`] to perform
//!   the extra ping/pong round trip; the RTT is returned in
//!   [`PingResult::latency`].
//! * **MOTD conversion** — [`StatusResponse::description`] is rendered into
//!   `§`-coded legacy text as the status deserializes, whether the server sent
//!   a plain string or a Chat-component object ([`chat::to_legacy_text`] does
//!   the same to any component, [`chat::to_plain_text`] drops the styling
//!   instead).
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
//!     // Already `§`-coded, colors and styles included.
//!     println!("motd: {}", result.status.description);
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
//!   `description` is the exception to "typed": a server sends it as a plain
//!   string or as a Chat-component object, and typing it as a component would
//!   lose the whole status on malformed server JSON. It is rendered into
//!   `§`-coded legacy text as it deserializes ([`chat::to_legacy_text`]), so
//!   the field holds what a client draws whatever shape arrived.

pub mod chat;
mod error;
mod models;
mod protocol;
mod srv;

use std::time::Duration;

use tokio::io::{AsyncWriteExt, BufStream};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout_at};

pub use error::Error;
pub use models::{
    DEFAULT_PORT, PingOptions, PingResult, PlayerSample, Players, ResolvedAddress, ServerAddress,
    StatusResponse, Version,
};

/// Resolve the effective address to connect to, applying SRV lookup when the
/// address leaves room for it.
///
/// An address with an explicit port, or an IP literal, is already complete and
/// is returned as-is — SRV could only contradict it. A bare hostname is looked
/// up as `_minecraft._tcp.<host>` and, when a usable record exists, resolved
/// to the record's target and port. With no record (or with
/// [`PingOptions::use_srv`] off) the host is used on
/// [`ServerAddress::effective_port`].
///
/// The result is also used internally by [`ping`]; the port may come from the
/// SRV record while the handshake keeps the original hostname.
pub async fn resolve_server_address(address: &ServerAddress) -> Result<ResolvedAddress, Error> {
    resolve_server_address_with_options(address, &PingOptions::default()).await
}

/// Same as [`resolve_server_address`] but honoring the caller's
/// [`PingOptions::use_srv`] flag.
pub async fn resolve_server_address_with_options(
    address: &ServerAddress,
    options: &PingOptions,
) -> Result<ResolvedAddress, Error> {
    if !options.use_srv || !address.allows_srv() {
        return Ok(direct(address));
    }

    // Reuse the process-wide resolver (shared DNS cache); on init failure or
    // when there is no usable SRV record, connect directly.
    if let Some(resolver) = srv::shared_resolver()
        && let Some(record) = srv::resolve_srv(resolver, &address.host).await?
    {
        return Ok(ResolvedAddress {
            host: record.target,
            port: record.port,
            used_srv: true,
        });
    }

    Ok(direct(address))
}

/// The address as given, used whenever no SRV record redirects it.
fn direct(address: &ServerAddress) -> ResolvedAddress {
    ResolvedAddress {
        host: address.host.clone(),
        port: address.effective_port(),
        used_srv: false,
    }
}

/// Ping a Minecraft Java server and (optionally) measure latency.
///
/// The status exchange must fit inside [`PingOptions::timeout`]; the latency
/// round trip is best-effort and reports `None` rather than failing the ping
/// (see [`PingResult::latency`]).
pub async fn ping(address: &ServerAddress, options: &PingOptions) -> Result<PingResult, Error> {
    let deadline = Instant::now() + options.timeout;
    let resolved = timeout_at(
        deadline,
        resolve_server_address_with_options(address, options),
    )
    .await
    .map_err(|_| Error::Timeout(options.timeout))??;
    timeout_at(
        deadline,
        ping_inner(&resolved, &address.host, options, deadline),
    )
    .await
    .map_err(|_| Error::Timeout(options.timeout))?
}

async fn ping_inner(
    resolved: &ResolvedAddress,
    original_host: &str,
    options: &PingOptions,
    deadline: Instant,
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

    // The status is in hand: from here on nothing may fail the ping. The extra
    // round trip only gets the time left on the clock, and any failure — a
    // server that hangs up, ignores the ping, or outlives the deadline — costs
    // the latency instead of the result.
    let latency = if options.measure_latency {
        match timeout_at(deadline, ping_round_trip(&mut stream, options)).await {
            Ok(Ok(rtt)) => Some(rtt),
            Ok(Err(_)) | Err(_) => None,
        }
    } else {
        None
    };

    Ok(PingResult { status, latency })
}

/// The extra ping/pong exchange (packet id `0x01`), returning the round trip.
async fn ping_round_trip(
    stream: &mut BufStream<TcpStream>,
    options: &PingOptions,
) -> Result<Duration, Error> {
    stream
        .write_all(&protocol::build_ping_packet(protocol::now_nanos())?)
        .await?;
    stream.flush().await?;
    let start = Instant::now();
    let pong_frame = protocol::read_frame(stream, options.max_frame_size).await?;
    protocol::parse_pong_frame(&pong_frame)?;
    Ok(start.elapsed())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards against accidentally making the public futures `!Send` (e.g. by
    /// borrowing a thread-local RNG across `.await`), which would break
    /// `tokio::spawn` for downstream users.
    #[test]
    fn public_futures_are_send() {
        fn assert_send<T: Send>(_: T) {}
        let address = ServerAddress::new("127.0.0.1", 25565);
        let options = PingOptions::default();
        assert_send(ping(&address, &options));
        assert_send(resolve_server_address(&address));
    }

    /// An explicit port is an address in its own right: resolution must not
    /// consult SRV, which could only contradict the caller. Runs no DNS.
    #[tokio::test]
    async fn explicit_port_is_used_as_given() {
        let address: ServerAddress = "mc233.cn:1234".parse().unwrap();
        let resolved = resolve_server_address(&address).await.unwrap();
        assert_eq!(resolved.host, "mc233.cn");
        assert_eq!(resolved.port, 1234);
        assert!(!resolved.used_srv);
    }

    /// Status response the fake server answers with.
    const STATUS_JSON: &str = r#"{"version":{"name":"1.21.4","protocol":769},"players":{"max":10,"online":1},"description":"Hi"}"#;

    /// Largest frame the fake server accepts.
    const FRAME_LIMIT: u32 = 1024 * 1024;

    /// Serve one status exchange on a loopback port and return that port.
    ///
    /// The handshake and status request are answered with [`STATUS_JSON`], then
    /// the ping is read and — `hang_up` picking between them — the connection
    /// is either closed (the client sees EOF) or left open (the client waits
    /// for its deadline). The pong is never sent, in both cases.
    async fn serve_status_without_pong(hang_up: bool) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            protocol::read_frame(&mut socket, FRAME_LIMIT)
                .await
                .unwrap(); // handshake
            protocol::read_frame(&mut socket, FRAME_LIMIT)
                .await
                .unwrap(); // status request
            socket
                .write_all(&protocol::build_status_frame(STATUS_JSON).unwrap())
                .await
                .unwrap();
            socket.flush().await.unwrap();
            protocol::read_frame(&mut socket, FRAME_LIMIT)
                .await
                .unwrap(); // ping
            if hang_up {
                return; // dropping the socket closes it
            }
            std::future::pending::<()>().await;
        });
        port
    }

    fn measured_options(timeout: Duration) -> PingOptions {
        PingOptions {
            measure_latency: true,
            timeout,
            ..Default::default()
        }
    }

    /// A server that hangs up after the status must still deliver its status:
    /// the round trip is best-effort, so the failed measurement reports `None`
    /// instead of discarding the response.
    #[tokio::test]
    async fn latency_failure_still_returns_status() {
        let port = serve_status_without_pong(true).await;
        let result = ping(
            &ServerAddress::new("127.0.0.1", port),
            &measured_options(Duration::from_secs(5)),
        )
        .await
        .unwrap();
        assert_eq!(result.status.version.name, "1.21.4");
        assert_eq!(result.status.description, "Hi");
        assert!(result.latency.is_none(), "a failed round trip reports None");
    }

    /// Same for a server that keeps the connection open and never answers the
    /// ping: running out of time costs the latency, not the result.
    #[tokio::test]
    async fn latency_timeout_still_returns_status() {
        let port = serve_status_without_pong(false).await;
        let result = ping(
            &ServerAddress::new("127.0.0.1", port),
            &measured_options(Duration::from_millis(250)),
        )
        .await
        .unwrap();
        assert_eq!(result.status.version.name, "1.21.4");
        assert!(result.latency.is_none(), "an unanswered ping reports None");
    }
}
