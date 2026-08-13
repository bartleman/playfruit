//! Playfruit CLI — stream system audio to a HomePod over AirPlay 2.
//!
//!   playfruit <ip> [--volume 0.5] [--latency gaming|video|music] [--name NAME]
//!
//! Thin front-end over the supervised session engine (see `engine.rs`):
//! silence keepalive, drift regulation and auto-reconnect included.

use std::net::IpAddr;

use playfruit_cli::{Engine, EngineConfig, EngineStatus};
use cap_core::streaming::LatencyProfile;

mod doctor;

/// Where the apps write their logs — printed by `doctor` for bug reports.
fn log_dir_hint() -> String {
    let base = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("playfruit").display().to_string()
}

struct Args {
    target: String,
    mute_local: bool,
    volume: f32,
    latency: LatencyProfile,
    name: String,
}

fn parse_args() -> Result<Args, String> {
    let mut ip = None;
    let mut volume = 0.5f32;
    let mut latency = LatencyProfile::Video;
    let mut mute_local = true;
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
            "--keep-pc-audio" => mute_local = false,
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
                    ip = Some(other.to_string());
                } else {
                    return Err(format!("unexpected argument: {other}"));
                }
            }
        }
    }
    let target = ip.ok_or("usage: playfruit <ip-or-name> [--volume 0.5] [--latency gaming|video|music]")?;
    Ok(Args {
        name: name.unwrap_or_else(|| format!("HomePod {target}")),
        target,
        mute_local,
        volume: volume.clamp(0.0, 1.0),
        latency,
    })
}

fn main() {
    // `playfruit doctor [ip]` — diagnostic mode, quiet by default (no
    // tracing subscriber: check output IS the diagnosis).
    if std::env::args().nth(1).as_deref() == Some("doctor") {
        std::process::exit(doctor::run(std::env::args().nth(2)));
    }

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

    // Accept a device name ("kitchen") as well as an IP: resolve via mDNS.
    let (ip, resolved_name) = match args.target.parse::<IpAddr>() {
        Ok(ip) => (ip, None),
        Err(_) => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let devices = rt
                .block_on(cap_core::discovery::browse_once(
                    std::time::Duration::from_secs(3),
                ))
                .unwrap_or_default();
            let needle = args.target.to_lowercase();
            match devices.iter().find(|d| {
                d.supports_airplay2
                    && !d.name.contains('@')
                    && d.name.to_lowercase().contains(&needle)
            }) {
                Some(d) => {
                    let addr = d
                        .addresses
                        .iter()
                        .find(|a| a.is_ipv4())
                        .or_else(|| d.addresses.first())
                        .copied();
                    match addr {
                        Some(a) => {
                            println!("found {} at {a}", d.name);
                            (a, Some(d.name.clone()))
                        }
                        None => {
                            eprintln!("device '{}' found but has no usable address", d.name);
                            std::process::exit(2);
                        }
                    }
                }
                None => {
                    eprintln!(
                        "no AirPlay 2 device matching '{}' found — try 'playfruit doctor' to list devices",
                        args.target
                    );
                    std::process::exit(2);
                }
            }
        }
    };

    let (engine, status_rx) = Engine::start(EngineConfig {
        ip,
        port: 7000,
        name: resolved_name.unwrap_or(args.name),
        volume: args.volume,
        latency: args.latency,
        mute_local: args.mute_local,
    });

    // Print status transitions until Ctrl-C or a terminal state.
    let printer = std::thread::spawn(move || {
        while let Ok(st) = status_rx.recv() {
            match st {
                EngineStatus::Connecting { name } => println!("connecting to {name}…"),
                EngineStatus::Streaming { name } => {
                    println!("✓ streaming to {name} — Ctrl-C to stop")
                }
                EngineStatus::StreamingSilent { name } => {
                    println!("· connected to {name} — nothing is playing on this PC")
                }
                EngineStatus::Warning { name, message } => {
                    eprintln!("⚠ {name}: {message}")
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
