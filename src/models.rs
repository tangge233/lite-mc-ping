//! Public data types: server address, options, and parsed status response.

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Default Minecraft Java Edition port.
pub const DEFAULT_PORT: u16 = 25565;

/// A server to ping: hostname (or IP literal) plus an optional port.
///
/// Parseable from `"host"`, `"host:port"`, `"[::1]"` and `"[::1]:port"` forms
/// via [`FromStr`].
///
/// A port that was given is used as-is, including `":25565"`, while an omitted
/// port is a default that SRV resolution may replace with the port of the
/// `_minecraft._tcp.<host>` record (see [`crate::resolve_server_address`]).
/// Write `"play.example.com"` rather than `"play.example.com:25565"` to let an
/// SRV record route the ping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerAddress {
    /// Hostname or IP literal. Only the address syntax is checked here; name
    /// resolution happens at connect time.
    pub host: String,
    /// TCP port as given by the caller, or `None` when the input omitted it —
    /// then [`ServerAddress::effective_port`] reports [`DEFAULT_PORT`].
    pub port: Option<u16>,
}

impl ServerAddress {
    /// Address with an explicit port: used as-is, never SRV-resolved.
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port: Some(port),
        }
    }

    /// Address without a port: connects to [`DEFAULT_PORT`], or to the port of
    /// the `_minecraft._tcp.<host>` SRV record when one exists.
    pub fn without_port(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: None,
        }
    }

    /// Port to connect to when no SRV record applies.
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or(DEFAULT_PORT)
    }

    /// Whether SRV resolution may replace this address: only a bare hostname
    /// leaves anything for a record to fill in.
    pub(crate) fn allows_srv(&self) -> bool {
        self.port.is_none() && self.host.parse::<IpAddr>().is_err()
    }
}

impl FromStr for ServerAddress {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty address".into());
        }
        // Bracket form handles IPv6 explicitly: "[::1]:25565" or "[::1]".
        if let Some(rest) = s.strip_prefix('[') {
            let (host, tail) = rest
                .split_once(']')
                .ok_or_else(|| format!("missing ']' in {s:?}"))?;
            if host.is_empty() {
                return Err(format!("missing host in {s:?}"));
            }
            let port = match tail {
                "" => None,
                tail => {
                    Some(parse_port(tail.strip_prefix(':').ok_or_else(|| {
                        format!("unexpected {tail:?} after ']' in {s:?}")
                    })?)?)
                }
            };
            return Ok(ServerAddress {
                host: host.into(),
                port,
            });
        }
        // A bare IP literal (notably IPv6, as in "::1") never carries a port.
        if let Ok(ip) = s.parse::<IpAddr>() {
            return Ok(ServerAddress::without_port(ip.to_string()));
        }
        // "host" or "host:port": whatever follows the last colon is the port,
        // so a host still holding a colon is malformed — IPv6 needs brackets.
        let (host, port) = match s.rsplit_once(':') {
            Some((host, port)) => (host, Some(parse_port(port)?)),
            None => (s, None),
        };
        if host.is_empty() {
            return Err(format!("missing host in {s:?}"));
        }
        if host.contains(':') {
            return Err(format!(
                "invalid host {host:?} in {s:?}: write IPv6 literals in brackets"
            ));
        }
        Ok(ServerAddress {
            host: host.into(),
            port,
        })
    }
}

/// Parse a port number, rejecting empty, non-numeric and out-of-range values.
fn parse_port(port: &str) -> Result<u16, String> {
    port.parse().map_err(|_| format!("invalid port {port:?}"))
}

impl fmt::Display for ServerAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Formats as the address was given, so that parsing a displayed
        // address yields the same one again. IPv6 literals are bracketed.
        let (open, close) = if self.host.parse::<Ipv6Addr>().is_ok() {
            ("[", "]")
        } else {
            ("", "")
        };
        match self.port {
            Some(port) => write!(f, "{open}{}{close}:{port}", self.host),
            None => write!(f, "{open}{}{close}", self.host),
        }
    }
}

/// Tunables for a single ping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PingOptions {
    /// Protocol version sent in the handshake. `-1` means "latest" and is
    /// accepted by virtually all modern servers. Encoded as a plain unsigned
    /// varint of the two's-complement value.
    pub protocol_version: i32,
    /// Whether to perform the extra ping/pong exchange (packet id `0x01`) and
    /// report the round-trip latency in [`PingResult::latency`]. Best-effort:
    /// a failed or slow exchange leaves the latency `None` without failing the
    /// ping.
    pub measure_latency: bool,
    /// Overall timeout for the whole operation. The status exchange must fit
    /// inside it; the latency round trip only gets whatever time is left, and
    /// running out of it reports [`PingResult::latency`] `None` rather than
    /// [`crate::Error::Timeout`].
    pub timeout: Duration,
    /// Maximum accepted frame size; guards against excessive declared lengths.
    pub max_frame_size: u32,
    /// Whether to attempt `_minecraft._tcp.<host>` SRV lookup. Never applied
    /// to an IP literal or to an address with an explicit port, which are
    /// already complete; on lookup failure the ping falls back to the address
    /// as given.
    pub use_srv: bool,
}

impl Default for PingOptions {
    fn default() -> Self {
        Self {
            protocol_version: -1,
            measure_latency: false,
            timeout: Duration::from_secs(5),
            max_frame_size: 1024 * 1024,
            use_srv: true,
        }
    }
}

/// Result of a ping.
#[derive(Debug, Clone, PartialEq)]
pub struct PingResult {
    /// Parsed status response JSON.
    pub status: StatusResponse,
    /// Ping/pong round-trip time.
    ///
    /// `None` when [`PingOptions::measure_latency`] was off, and also when the
    /// round trip failed or ran out of the operation's time: the measurement is
    /// best-effort, so a server that hangs up or ignores the ping still yields
    /// [`PingResult::status`].
    pub latency: Option<Duration>,
}

/// The server's `status` JSON payload.
///
/// Parsed leniently: unknown fields are ignored and the optional fields below
/// default when missing, so responses from older or modded servers still
/// deserialize. Non-exhaustive: the protocol evolves, new fields are expected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StatusResponse {
    pub version: Version,
    #[serde(default)]
    pub players: Players,
    /// The MOTD. Either a plain string or a Chat-component object
    /// (`{"text": "..."}` or `{"extra": [...]}`), so it is kept as raw JSON;
    /// [`crate::chat::to_legacy_text`] converts it to `§`-coded legacy text
    /// and [`crate::chat::to_plain_text`] to plain text.
    pub description: serde_json::Value,
    #[serde(default)]
    pub favicon: Option<String>,
    #[serde(default, rename = "enforcesSecureChat")]
    pub enforces_secure_chat: Option<bool>,
    #[serde(default, rename = "previewsChat")]
    pub previews_chat: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Version {
    /// Display name, e.g. `"1.21.4"`.
    pub name: String,
    /// Wire protocol number. Defaults to `0` if the server omits it.
    #[serde(default)]
    pub protocol: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Players {
    pub max: u32,
    pub online: u32,
    #[serde(default)]
    pub sample: Vec<PlayerSample>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PlayerSample {
    pub name: String,
    /// UUID of the player, typically without dashes.
    pub id: String,
}

/// Address actually used after (optional) SRV resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAddress {
    /// Hostname or IP literal to connect to.
    pub host: String,
    /// Port to connect to (SRV port when a record was used).
    pub port: u16,
    /// Whether an SRV record redirected the connection.
    pub used_srv: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_host_only_leaves_port_open() {
        let a: ServerAddress = "mc.example.com".parse().unwrap();
        assert_eq!(a, ServerAddress::without_port("mc.example.com"));
        assert_eq!(a.port, None);
        assert_eq!(a.effective_port(), DEFAULT_PORT);
    }

    #[test]
    fn parse_host_port_is_explicit() {
        let a: ServerAddress = "mc.example.com:25566".parse().unwrap();
        assert_eq!(a, ServerAddress::new("mc.example.com", 25566));
        assert_eq!(a.port, Some(25566));
    }

    #[test]
    fn parse_bracketed_ipv6() {
        let a: ServerAddress = "[::1]:25565".parse().unwrap();
        assert_eq!(a, ServerAddress::new("::1", 25565));
        let b: ServerAddress = "[2001:db8::5]".parse().unwrap();
        assert_eq!(b, ServerAddress::without_port("2001:db8::5"));
    }

    #[test]
    fn parse_bare_ipv6() {
        let a: ServerAddress = "::1".parse().unwrap();
        assert_eq!(a, ServerAddress::without_port("::1"));
    }

    #[test]
    fn display_formats_ipv6_with_brackets() {
        assert_eq!(ServerAddress::new("::1", 25565).to_string(), "[::1]:25565");
        assert_eq!(ServerAddress::without_port("::1").to_string(), "[::1]");
        assert_eq!(
            ServerAddress::new("mc.example.com", 25566).to_string(),
            "mc.example.com:25566"
        );
    }

    /// Displaying what was parsed — and not a defaulted port — keeps the SRV
    /// decision intact across a round trip.
    #[test]
    fn display_round_trips() {
        for input in [
            "mc.example.com",
            "mc.example.com:25566",
            "[::1]",
            "[::1]:25565",
        ] {
            let address: ServerAddress = input.parse().unwrap();
            assert_eq!(address.to_string(), input);
            assert_eq!(
                address.to_string().parse::<ServerAddress>().unwrap(),
                address
            );
        }
    }

    /// Only a bare hostname leaves the port open for SRV to fill in.
    #[test]
    fn srv_applies_only_to_bare_hostnames() {
        for input in ["mc.example.com", "mc233.cn"] {
            assert!(
                input.parse::<ServerAddress>().unwrap().allows_srv(),
                "{input}"
            );
        }
        for input in [
            "mc.example.com:25565",
            "mc.example.com:25566",
            "1.2.3.4",
            "1.2.3.4:25565",
            "[::1]",
            "[::1]:25565",
        ] {
            assert!(
                !input.parse::<ServerAddress>().unwrap().allows_srv(),
                "{input}"
            );
        }
    }

    #[test]
    fn invalid_port_is_rejected() {
        assert!("mc.example.com:port".parse::<ServerAddress>().is_err());
        assert!("[::1]:abc".parse::<ServerAddress>().is_err());
        assert!("".parse::<ServerAddress>().is_err());
    }

    /// Malformed addresses are reported rather than silently turning the port
    /// into part of the hostname, which would fail later as a DNS error.
    #[test]
    fn malformed_addresses_are_rejected() {
        for input in [
            "mc.example.com:",
            ":123",
            "mc.example.com:70000",
            "mc.example.com:25565:99",
            "[::1]:",
            "[::1]junk",
            "[]",
        ] {
            assert!(input.parse::<ServerAddress>().is_err(), "{input}");
        }
    }

    #[test]
    fn options_defaults() {
        let o = PingOptions::default();
        assert_eq!(o.protocol_version, -1);
        assert!(!o.measure_latency);
        assert_eq!(o.timeout, Duration::from_secs(5));
        assert!(o.use_srv);
    }
}
