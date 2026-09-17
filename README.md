# lite-mc-ping

Minimal async **Minecraft Java Edition (1.7+)** server list ping for Rust, on tokio.

```
Client → Server: Handshake    (0x00) protocol, host, port, next_state=1
Client → Server: Status req   (0x00)
Server → Client: Status resp  (0x00) JSON: version / players / description …
Client → Server: Ping         (0x01) i64 timestamp            [optional]
Server → Client: Pong         (0x01) same timestamp           [optional]
```

## Features

- **SRV resolution** — `_minecraft._tcp.<host>` via `hickory-resolver` with RFC 2782
  weighted-random target selection. Falls back to the direct address on lookup
  failure, and is skipped for IP literals. When an SRV record is used the
  handshake still carries the *original* hostname (Mojang client behavior).
- **Optional latency** — `PingOptions::measure_latency` runs the extra ping/pong
  round trip and returns the RTT as [`PingResult::latency`].
- **Async API** on tokio; `hickory-resolver` + `varint` + `serde_json` under the hood.

## Usage

```toml
[dependencies]
lite-mc-ping = "0.1"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

```rust
use lite_mc_ping::{ping, PingOptions, ServerAddress};

#[tokio::main]
async fn main() {
    let address: ServerAddress = "play.example.com:25565".parse().unwrap();
    let options = PingOptions { measure_latency: true, ..Default::default() };
    let result = ping(&address, &options).await.unwrap();
    println!("{} / {} online — {}", result.status.players.online,
             result.status.players.max, result.status.description);
    if let Some(ms) = result.latency {
        println!("latency: {}ms", ms.as_millis());
    }
}
```

`ServerAddress` parses `"host"`, `"host:port"` and `"[::1]:25565"` (default port 25565).

## Example CLI

```console
$ cargo run --example ping -- --latency play.hypixel.net
pinging play.hypixel.net:25565 …
host:        play.hypixel.net:25565
version:     Requires MC 1.8 / 1.21 (protocol 47)
players:     16033/200000
motd:        §f                 §aHypixel Network §c[1.8/26.3]
favicon:     15738 bytes (base64)
latency:     210.869998ms
```

## Design notes

| Decision | Why |
| --- | --- |
| Only the *modern* (1.7+) protocol | Legacy ≤1.6 ping, proxying and mod-list parsing are out of scope |
| [`varint`] crate for VarInts | But only its *unsigned* methods — its signed methods are protobuf **zigzag**, which does not match Minecraft (protocol `-1` must encode as `FF FF FF FF 0F`). The frame-length VarInt, which must be read incrementally from the async stream, is a tiny 5-byte-capped loop since the crate is `std::io`-only |
| Typed status JSON | `StatusResponse` structs, kept lenient: unknown fields ignored, optional fields defaulted; `description` stays `serde_json::Value` (string or Chat-component object) |
| RFC 2782 weighted SRV pick | weight-proportional random within the lowest-priority group; uniform when all weights are 0; root (`"."`) targets skipped |
| Timeout + frame-size cap | Whole operation wrapped in `PingOptions::timeout`; frames capped at `PingOptions::max_frame_size` (1 MiB default) |

## Tests

`cargo test` runs unit tests (VarInt/packet encoding, RFC 2782 selection) plus
end-to-end integration tests against an in-process fake Minecraft server
(status fields, latency on/off, timeout, frame-size limit, plain-string MOTD).

## License

MIT