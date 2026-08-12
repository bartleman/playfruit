//! Playfruit CLI — stream system audio to a HomePod over AirPlay 2.
//!
//!   playfruit <ip> [--volume 0.5] [--latency gaming|video|music] [--name NAME]
//!
//! Thin front-end over the supervised session engine (see `engine.rs`):
//! silence keepalive, drift regulation and auto-reconnect included.

use std::net::IpAddr;

use playfruit_cli::{Engine, EngineConfig, EngineStatus};
use cap_core::streaming::LatencyProfile;

struct Args {
    ip: IpAddr,
    volume: f32,
    latency: LatencyProfile,
    name: String,
}

fn parse_args() -> Result<Args, String> {
    let mut ip = None;
    let mut volume = 0.5f32;
    let mut latency = LatencyProfile::Gaming;
    let mut name = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--volume" | "-v" => {
                volume = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .ok_or("--volume requires a number 0.0-1.0")?;
            }
            "--latency" | "-l" => {
                latency = match it.next().as_deref() {
                    Some("gaming") => LatencyProfile::Gaming,
                    Some("video") => LatencyProfile::Video,
                    Some("music") => LatencyProfile::Music,
                    other => {
                        return Err(format!("--latency must be gaming|video|music, got {other:?}"))
                    }
                };
            }
            "--name" | "-n" => name = it.next(),
            "--help" | "-h" => {
                println!("usage: playfruit <ip> [--volume 0.5] [--latency gaming|video|music] [--name NAME]");
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("playfruit {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => {
                if ip.is_none() {
                    ip = Some(other.parse().map_err(|_| format!("invalid IP: {other}"))?);
                } else {
                    return Err(format!("unexpected argument: {other}"));
                }
            }
        }
    }
    let ip = ip.ok_or("usage: playfruit <ip> [--volume 0.5] [--latency gaming|video|music]")?;
    Ok(Args {
        ip,
        volume: volume.clamp(0.0, 1.0),
        latency,
        name: name.unwrap_or_else(|| format!("HomePod {ip}")),
    })
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };

    let (engine, status_rx) = Engine::start(EngineConfig {
        ip: args.ip,
        port: 7000,
        name: args.name,
        volume: args.volume,
        latency: args.latency,
    });

    // Print status transitions until Ctrl-C or a terminal state.
    let printer = std::thread::spawn(move || {
        while let Ok(st) = status_rx.recv() {
            match st {
                EngineStatus::Connecting { name } => println!("connecting to {name}…"),
                EngineStatus::Streaming { name } => {
                    println!("✓ streaming to {name} — Ctrl-C to stop")
                }
                EngineStatus::Reconnecting { name, attempt } => {
                    println!("↻ reconnecting to {name} (attempt {attempt})…")
                }
                EngineStatus::Failed(e) => {
                    eprintln!("✗ {e}");
                    std::process::exit(1);
                }
                EngineStatus::Stopped => break,
            }
        }
    });

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let _ = tokio::signal::ctrl_c().await;
    });
    println!("\nstopping…");
    engine.stop();
    let _ = printer.join();
    // Hard exit: the vendored retransmit loop has no shutdown and would keep
    // the process alive after teardown.
    std::process::exit(0);
}
