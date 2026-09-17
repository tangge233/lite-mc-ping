//! Public data types: server address, options, and parsed status response.

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Default Minecraft Java Edition port.
pub const DEFAULT_PORT: u16 = 25565;

/// A server to ping: hostname (or IP literal) plus port.
///
/// Parseable from `"host"`, `"host:port"` and `"[::1]:port"` forms via
/// [`FromStr`]; bare strings default to [`DEFAULT_PORT`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerAddress {
    /// Hostname or IP literal. Allowed to contain underscores etc.; only
    /// used verbatim in the handshake and for connection.
    pub host: String,
    /// TCP port.
    pub port: u16,
}

impl ServerAddress {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
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
            let port = match tail.strip_prefix(':') {
                Some(p) if !p.is_empty() => p.parse().map_err(|_| format!("invalid port {p:?}"))?,
                _ => DEFAULT_PORT,
            };
            return Ok(ServerAddress::new(host, port));
        }
        // A bare string that is already an IP (e.g. "::1") → default port.
        if let Ok(ip) = s.parse::<IpAddr>() {
            return Ok(ServerAddress::new(ip.to_string(), DEFAULT_PORT));
        }
        // "host" or "host:port" — split on the last colon (hosts may contain
        // colons only in IPv6, already handled above).
        match s.rsplit_once(':') {
            Some((host, p)) if !host.is_empty() && !p.is_empty() => {
                let port = p.parse().map_err(|_| format!("invalid port {p:?}"))?;
                Ok(ServerAddress::new(host, port))
            }
            _ => Ok(ServerAddress::new(s, DEFAULT_PORT)),
        }
    }
}

impl fmt::Display for ServerAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.parse::<Ipv6Addr>().is_ok() {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
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
    /// report the round-trip latency in [`PingResult::latency`].
    pub measure_latency: bool,
    /// Overall timeout for the whole operation.
    pub timeout: Duration,
    /// Maximum accepted frame size, guarding against absurd declared lengths.
    pub max_frame_size: u32,
    /// Whether to attempt `_minecraft._tcp.<host>` SRV lookup. Never applied
    /// to IP literals; on lookup failure the ping falls back to the direct
    /// address.
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
    /// Ping/pong round-trip time, present only when
    /// [`PingOptions::measure_latency`] was enabled.
    pub latency: Option<Duration>,
}

/// The server's `status` JSON payload.
///
/// Parsed leniently: unknown fields are ignored and the optional fields below
/// default when missing, so responses from older or modded servers still
/// deserialize.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusResponse {
    pub version: Version,
    #[serde(default)]
    pub players: Players,
    /// The MOTD. Either a plain string or a Chat-component object
    /// (`{"text": "..."}` or `{"extra": [...]}`), so it is kept as raw JSON.
    pub description: serde_json::Value,
    #[serde(default)]
    pub favicon: Option<String>,
    #[serde(default, rename = "enforcesSecureChat")]
    pub enforces_secure_chat: Option<bool>,
    #[serde(default, rename = "previewsChat")]
    pub previews_chat: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    /// Display name, e.g. `"1.21.4"`.
    pub name: String,
    /// Wire protocol number. Defaults to `0` if the server omits it.
    #[serde(default)]
    pub protocol: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Players {
    pub max: u32,
    pub online: u32,
    #[serde(default)]
    pub sample: Vec<PlayerSample>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    fn parse_host_only_defaults_port() {
        let a: ServerAddress = "mc.example.com".parse().unwrap();
        assert_eq!(a, ServerAddress::new("mc.example.com", DEFAULT_PORT));
    }

    #[test]
    fn parse_host_port() {
        let a: ServerAddress = "mc.example.com:25566".parse().unwrap();
        assert_eq!(a, ServerAddress::new("mc.example.com", 25566));
    }

    #[test]
    fn parse_bracketed_ipv6() {
        let a: ServerAddress = "[::1]:25565".parse().unwrap();
        assert_eq!(a, ServerAddress::new("::1", 25565));
        let b: ServerAddress = "[2001:db8::5]".parse().unwrap();
        assert_eq!(b, ServerAddress::new("2001:db8::5", DEFAULT_PORT));
    }

    #[test]
    fn parse_bare_ipv6() {
        let a: ServerAddress = "::1".parse().unwrap();
        assert_eq!(a, ServerAddress::new("::1", DEFAULT_PORT));
    }

    #[test]
    fn display_formats_ipv6_with_brackets() {
        let a = ServerAddress::new("::1", 25565);
        assert_eq!(a.to_string(), "[::1]:25565");
        assert_eq!(
            ServerAddress::new("mc.example.com", 25566).to_string(),
            "mc.example.com:25566"
        );
    }

    #[test]
    fn invalid_port_is_rejected() {
        assert!("mc.example.com:port".parse::<ServerAddress>().is_err());
        assert!("[::1]:abc".parse::<ServerAddress>().is_err());
        assert!("".parse::<ServerAddress>().is_err());
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
