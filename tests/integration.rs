//! End-to-end tests against real Minecraft servers.
//!
//! Network-dependent. Servers are listed in [`SERVERS`] and pinged in
//! sequence; assertions target stable structural facts (fields parse, counts
//! are sane, SRV records redirect the connection), never exact values.

use std::time::Duration;

use lite_mc_ping::{
    Error, PingOptions, PingResult, ServerAddress, ping, resolve_server_address,
    resolve_server_address_with_options,
};

/// Servers exercised by the network tests, in the order they are pinged.
const SERVERS: &[&str] = &["mc233.cn", "mc.saltwood.top", "mc.hypixel.net"];

fn address(host: &str) -> ServerAddress {
    ServerAddress::new(host, 25565)
}

fn options() -> PingOptions {
    PingOptions {
        timeout: Duration::from_secs(10),
        ..Default::default()
    }
}

fn assert_sane_status(result: &PingResult) {
    let status = &result.status;
    assert!(!status.version.name.is_empty(), "empty version name");
    assert!(status.players.max > 0, "player max should be positive");
    assert!(
        status.description.is_string() || status.description.is_object(),
        "unexpected description shape"
    );
}

/// Ping every server in [`SERVERS`]: SRV resolution first, then the status
/// exchange, asserting a parsable result each time.
#[tokio::test]
async fn pings_all_servers_in_sequence() {
    for host in SERVERS {
        let resolved = resolve_server_address(host, 25565)
            .await
            .unwrap_or_else(|e| panic!("{host}: resolve failed: {e}"));
        assert!(resolved.port > 0, "{host}: resolved port must be valid");

        let result = ping(&address(host), &options())
            .await
            .unwrap_or_else(|e| panic!("{host}: ping failed: {e}"));
        assert_sane_status(&result);
        assert!(result.latency.is_none(), "{host}: latency off by default");
    }
}

#[tokio::test]
async fn measures_latency_on_real_server() {
    let opts = PingOptions {
        measure_latency: true,
        ..options()
    };
    let result = ping(&address(SERVERS[0]), &opts).await.unwrap();
    assert_sane_status(&result);
    assert!(result.latency.is_some(), "latency should be measured");
}

/// With SRV disabled, the first server in the set must be contacted at its
/// original address and still answer.
#[tokio::test]
async fn srv_disabled_connects_to_original_address() {
    let host = SERVERS[0];
    let opts = PingOptions {
        use_srv: false,
        ..options()
    };

    let resolved = resolve_server_address_with_options(host, 25565, &opts)
        .await
        .unwrap();
    assert!(!resolved.used_srv);
    assert_eq!(resolved.host, host);
    assert_eq!(resolved.port, 25565);

    let result = ping(&address(host), &opts).await.unwrap();
    assert_sane_status(&result);
}

/// A sub-millisecond timeout must abort the operation with `Error::Timeout`.
#[tokio::test]
async fn tiny_timeout_fails_fast() {
    let opts = PingOptions {
        timeout: Duration::from_millis(1),
        ..options()
    };
    let err = ping(&address(SERVERS[0]), &opts).await.unwrap_err();
    assert!(matches!(err, Error::Timeout(_)), "got {err:?}");
}
