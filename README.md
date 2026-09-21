# lite-mc-ping

Minimal async Minecraft **Java Edition (1.7+)** server list ping crate for Rust, built on tokio.

Modern protocol flow over TCP:

```text
Client → Server: Handshake    (0x00) protocol, host, port, next_state=1
Client → Server: Status req   (0x00)
Server → Client: Status resp  (0x00) JSON: version / players / description …
Client → Server: Ping         (0x01) i64 timestamp           [optional]
Server → Client: Pong         (0x01) same timestamp          [optional]
```

## Features

- **SRV resolution** — `_minecraft._tcp.<host>` via `hickory-resolver`, with RFC 2782
  weighted-random target selection. Looked up only when the address leaves the port open:
  an explicit port (`play.example.com:25566`) or an IP literal is used as-is, and a failed
  or empty lookup falls back to the address as given. When an SRV record is used, the
  handshake keeps the *original* hostname (Mojang-client behavior).
- **Optional latency** — `PingOptions::measure_latency` runs the ping/pong round trip
  and reports the RTT in `PingResult::latency`. Best-effort: the exchange only gets the
  time left on the timeout, and a server that hangs up or never answers leaves the
  latency `None` while the status is still returned.
- **Typed response** — status JSON deserializes into `StatusResponse` (version,
  players, player sample, description, favicon, chat flags).
- **MOTD conversion** — `chat::to_legacy_text` renders the JSON Chat component into
  `§`-coded legacy text (colors incl. `#rrggbb` downgrade, styles, nested `extra`),
  `chat::to_plain_text` renders the same content unstyled. Shapes the legacy format
  cannot express (`score`/`selector`/`nbt`/`keybind`, 1.21.9+ `object` sprites) are
  skipped instead of failing.
- **Tokio-native async** — powered by `hickory-resolver` (DNS), `varint` (VarInt codec)
  and `serde_json`.

## Install

```toml
[dependencies]
lite-mc-ping = "0.1"
```

Requires Rust 1.85+ (edition 2024) and a tokio runtime.

## Usage

```rust
use lite_mc_ping::{chat, ping, PingOptions, ServerAddress};

#[tokio::main]
async fn main() {
    let address: ServerAddress = "play.example.com".parse().unwrap();
    let options = PingOptions {
        measure_latency: true,
        ..Default::default()
    };
    let result = ping(&address, &options).await.unwrap();

    let motd = chat::to_plain_text(&result.status.description);
    println!("{} / {} online — {motd}", result.status.players.online,
             result.status.players.max);
    let styled = chat::to_legacy_text(&result.status.description);
    println!("motd: {styled}");
    if let Some(latency) = result.latency {
        println!("latency: {} ms", latency.as_millis());
    }
}
```

`ServerAddress` parses `"host"`, `"host:port"`, `"[::1]"` and `"[::1]:port"`; a
port that is given is used as-is, including `":25565"`. An omitted port is only a
default, so write `"play.example.com"` and not `"play.example.com:25565"` to let an
`_minecraft._tcp` SRV record route the ping.

### API

| Item | Description |
| --- | --- |
| `ping(&ServerAddress, &PingOptions)` | Full status exchange; measures latency when enabled |
| `resolve_server_address(&ServerAddress)` | SRV lookup with fallback to the address as given |
| `ServerAddress::{new, without_port}` | Explicit port (never SRV-resolved) vs. bare hostname |
| `PingOptions` | `protocol_version` (default `-1`), `measure_latency`, `timeout`, `max_frame_size`, `use_srv` |
| `PingResult` / `StatusResponse` | Parsed response and (optional) RTT |
| `chat::to_legacy_text(&description)` | JSON Chat component → `§`-coded legacy text |
| `chat::to_plain_text(&description)` | Same content, styling dropped |

## Example CLI

```console
$ cargo run --example ping -- --latency play.hypixel.net
```

## Design notes

| Decision | Why |
| --- | --- |
| Modern (1.7+) protocol only | Legacy ≤1.6 ping, proxying and mod-list parsing are out of scope |
| `varint` crate, unsigned methods only | Its signed methods are protobuf **zigzag**, not Minecraft's two's-complement VarInt (protocol `-1` → `FF FF FF FF 0F`). The frame-length prefix — the only VarInt read from the async stream — uses a 5-byte-capped loop, as the crate is `std::io`-only |
| Lenient typed JSON | Unknown fields ignored, optional fields defaulted; `description` stays `serde_json::Value` (string or Chat-component object), and `chat` converts both shapes |
| Stateful legacy output | `§` codes persist until changed and a style can only be cleared by `§r`, so the writer tracks the state a client is in and emits a reset only when an attribute has to be dropped |
| Hex colors downgraded | `#rrggbb` maps to the nearest of the 16 legacy colors (vanilla behavior), not the BungeeCord `§x§r§r§g§g§b§b` extension, which vanilla clients do not understand |
| `translate` best-effort | The real text lives in the client's language files, so `fallback` is used when present, otherwise the `with` arguments joined by a space |
| SRV only fills in what was left out | Only a bare hostname leaves the port open, so an explicit port or IP literal is used as-is — a record could only contradict the caller. `PingOptions::use_srv = false` disables the lookup entirely |
| Latency is best-effort | The status is the valuable part of a ping, so a failed ping/pong (hang-up, no answer, deadline reached) reports `latency: None` instead of discarding a parsed status. The round trip only draws on the time left of `PingOptions::timeout` |
| Reused DNS resolver | One lazily built process-wide `hickory_resolver::Resolver` is shared across pings; it is `Clone + Sync` and its `moka` answer cache is shared on clone |
| RFC 2782 SRV selection | Weighted-random within the lowest-priority group; uniform when all weights are 0; root (`"."`) targets skipped |
| Timeout + size caps | Whole operation bounded by `PingOptions::timeout`; frames capped at `max_frame_size` (1 MiB default) |

## Tests

`cargo test` runs unit tests for VarInt/packet encoding, response parsing, RFC 2782
SRV selection and Chat-component conversion — including a loopback fake server that
covers the latency failure paths — plus network integration tests that ping the real
servers in `tests/integration.rs` (SRV resolution, status fields, latency, timeout).

## License

MIT — see [LICENSE](LICENSE).