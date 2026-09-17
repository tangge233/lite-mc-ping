//! Example CLI: `cargo run --example ping -- [--latency] <host>[:port]`

use std::time::Duration;

use lite_mc_ping::{PingOptions, ServerAddress, ping};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (address, options) = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(msg) => {
            eprintln!("error: {msg}");
            eprintln!("usage: ping [--latency] [-t <ms>] <host>[:port]");
            std::process::exit(2);
        }
    };

    println!("pinging {address} …");
    match ping(&address, &options).await {
        Ok(result) => print_result(&address, &result),
        Err(err) => {
            eprintln!("ping failed: {err}");
            std::process::exit(1);
        }
    }
}

fn parse_args(args: Vec<String>) -> Result<(ServerAddress, PingOptions), String> {
    let mut latency = false;
    let mut timeout = Duration::from_secs(5);
    let mut host: Option<String> = None;

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--latency" | "-l" => latency = true,
            "--timeout" | "-t" => {
                let ms = iter
                    .next()
                    .ok_or("--timeout requires a value in milliseconds")?
                    .parse::<u64>()
                    .map_err(|_| "invalid --timeout value")?;
                timeout = Duration::from_millis(ms);
            }
            flag if flag.starts_with('-') => return Err(format!("unknown flag {flag:?}")),
            value => host = Some(value.to_string()),
        }
    }

    let host = host.ok_or("missing host argument")?;
    let address: ServerAddress = host.parse().map_err(|e: String| e)?;
    let options = PingOptions {
        measure_latency: latency,
        timeout,
        ..Default::default()
    };
    Ok((address, options))
}

fn print_result(address: &ServerAddress, result: &lite_mc_ping::PingResult) {
    let s = &result.status;
    println!("host:        {address}");
    println!(
        "version:     {} (protocol {})",
        s.version.name, s.version.protocol
    );
    println!("players:     {}/{}", s.players.online, s.players.max);
    println!("motd:        {}", motd_text(&s.description));
    if let Some(favicon) = &s.favicon {
        println!("favicon:     {} bytes (base64)", favicon.len());
    }
    match result.latency {
        Some(latency) => println!("latency:     {latency:?}"),
        None => println!("latency:     not measured (use --latency)"),
    }
}

/// Render the description as plain text when possible: a bare string, or the
/// `text` field of a Chat component; otherwise fall back to compact JSON.
fn motd_text(description: &serde_json::Value) -> String {
    match description {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(map) => match map.get("text") {
            Some(serde_json::Value::String(s)) => s.clone(),
            _ => serde_json::to_string(description).unwrap_or_default(),
        },
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}
