//! `playfruit doctor [homepod-ip]` — one-paste diagnosis of the whole
//! pipeline: firewall rules, network profile, audio capture, discovery,
//! reachability, and (with an IP) a live session test that measures whether
//! the HomePod can actually reach our clock-sync responder.

use std::time::{Duration, Instant};

use audio_capture::CaptureFormat;

fn line(status: &str, name: &str, detail: &str) {
    println!("[{status}] {name} — {detail}");
}

/// `target`: an IP, a case-insensitive device-name fragment ("kitchen"), or
/// None — with None, the session test auto-runs only when exactly one
/// HomePod exists (never silently takes over a speaker among several).
pub fn run(target: Option<String>) -> i32 {
    println!("playfruit doctor v{}\n", env!("CARGO_PKG_VERSION"));
    let mut failures = 0;

    // D1: firewall rules (Windows only)
    #[cfg(windows)]
    {
        let out = std::process::Command::new("netsh")
            .args(["advfirewall", "firewall", "show", "rule", "name=Playfruit", "verbose"])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout).to_string();
                let all_profiles = text
                    .lines()
                    .filter(|l| l.trim_start().starts_with("Profiles:"))
                    .all(|l| l.contains("Any") || (l.contains("Public") && l.contains("Private")));
                let exe_dir = std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.display().to_string()))
                    .unwrap_or_default();
                let path_match = text.contains(&exe_dir);
                if all_profiles && path_match {
                    line("PASS", "firewall-rules", "Playfruit rules present, all profiles, program path matches");
                } else if !all_profiles {
                    failures += 1;
                    line("FAIL", "firewall-rules", "rules exist but do NOT cover all network profiles — on a Public-profile network the HomePod's clock-sync queries are blocked and it plays SILENCE. Fix: re-run 'Enable firewall access' in the tray menu (upgrades the rule).");
                } else {
                    failures += 1;
                    line("FAIL", "firewall-rules", &format!("rules exist but their program path does not match this exe's folder ({exe_dir}) — they cover a different/moved copy. Fix: re-run 'Enable firewall access'."));
                }
            }
            _ => {
                failures += 1;
                line("FAIL", "firewall-rules", "no 'Playfruit' firewall rule found. Fix: click 'Enable firewall access…' in the tray menu (one UAC prompt).");
            }
        }

        // D2: active network profile
        let prof = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", "(Get-NetConnectionProfile).NetworkCategory"])
            .output();
        match prof {
            Ok(o) if o.status.success() => {
                let cats = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if cats.is_empty() {
                    line("WARN", "network-profile", "could not determine the active network profile");
                } else {
                    line("INFO", "network-profile", &format!("active profile(s): {}", cats.replace('\n', ", ")));
                }
            }
            _ => line("WARN", "network-profile", "could not query Get-NetConnectionProfile"),
        }
    }
    #[cfg(not(windows))]
    line("SKIP", "firewall-rules", "Windows-only check");

    // D3: audio capture — 5 seconds of the real pipeline
    print!("[....] capture — sampling the default output for 5s");
    println!();
    match audio_capture::start_loopback(CaptureFormat::AIRPLAY_DEFAULT) {
        Ok((capture, rx)) => {
            let start = Instant::now();
            let mut frames: u64 = 0;
            let mut nonzero: u64 = 0;
            while start.elapsed() < Duration::from_secs(5) {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(f) => {
                        frames += (f.samples.len() / f.channels as usize) as u64;
                        if f.samples.iter().any(|&s| s != 0) {
                            nonzero += 1;
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(_) => {
                        failures += 1;
                        line("FAIL", "capture", "capture thread died mid-test (device error) — check the default output device");
                        break;
                    }
                }
            }
            capture.stop();
            let per_sec = frames as f64 / 5.0;
            if frames == 0 {
                line("WARN", "capture", "capture is alive but delivered 0 frames — nothing is playing on this PC (start some audio and re-run), or audio is routed to a non-default device");
            } else if nonzero == 0 {
                line("WARN", "capture", &format!("{per_sec:.0} frames/s captured but ALL silent — an app is rendering digital silence, or the wrong device is default"));
            } else {
                line("PASS", "capture", &format!("{per_sec:.0} frames/s (expect ~44100), real audio present"));
            }
        }
        Err(e) => {
            failures += 1;
            line("FAIL", "capture", &format!("could not open loopback capture: {e}"));
        }
    }

    // D4: discovery
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let devices = rt
        .block_on(cap_core::discovery::browse_once(Duration::from_secs(3)))
        .unwrap_or_default();
    // Prefer IPv4: link-local IPv6 confuses both users and connect attempts.
    let pick_addr = |d: &cap_core::discovery::Device| {
        d.addresses
            .iter()
            .find(|a| a.is_ipv4())
            .or_else(|| d.addresses.first())
            .copied()
    };
    let speakers: Vec<(String, std::net::IpAddr, cap_core::discovery::DeviceKind)> = devices
        .iter()
        .filter(|d| d.supports_airplay2 && !d.name.contains('@'))
        .filter_map(|d| pick_addr(d).map(|a| (d.name.clone(), a, d.kind)))
        .collect();
    if speakers.is_empty() {
        failures += 1;
        line("FAIL", "discovery", "no AirPlay 2 devices found in 3s — check that the PC and HomePod share a network and multicast/mDNS isn't blocked");
    } else {
        let listing: Vec<String> = speakers
            .iter()
            .map(|(n, a, k)| format!("{n} ({k:?}, {a})"))
            .collect();
        line("PASS", "discovery", &listing.join("; "));
    }

    // Resolve the session-test target: explicit IP, name fragment, or the
    // sole HomePod on the network.
    let homepods: Vec<_> = speakers
        .iter()
        .filter(|(_, _, k)| *k == cap_core::discovery::DeviceKind::HomePod)
        .collect();
    let ip: Option<std::net::IpAddr> = match &target {
        Some(t) => match t.parse::<std::net::IpAddr>() {
            Ok(ip) => Some(ip),
            Err(_) => {
                let needle = t.to_lowercase();
                match speakers
                    .iter()
                    .find(|(n, _, _)| n.to_lowercase().contains(&needle))
                {
                    Some((n, a, _)) => {
                        line("INFO", "target", &format!("matched '{t}' to {n} ({a})"));
                        Some(*a)
                    }
                    None => {
                        failures += 1;
                        line("FAIL", "target", &format!("no discovered device matches '{t}' — see the discovery list above"));
                        None
                    }
                }
            }
        },
        None => {
            if homepods.len() == 1 {
                let (n, a, _) = homepods[0];
                line("INFO", "target", &format!("one HomePod found — testing against {n} ({a})"));
                Some(*a)
            } else if homepods.len() > 1 {
                line(
                    "INFO",
                    "target",
                    &format!(
                        "{} HomePods found — re-run as 'playfruit doctor <name>' (e.g. doctor {}) to pick one for the live clock-sync test",
                        homepods.len(),
                        homepods[0].0.split_whitespace().next().unwrap_or("kitchen").to_lowercase()
                    ),
                );
                None
            } else {
                None
            }
        }
    };

    // D5 (+D6): live session test against the resolved device
    if let Some(ip) = ip {
        match std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::new(ip, 7000),
            Duration::from_secs(3),
        ) {
            Ok(_) => line("PASS", "reachability", &format!("{ip}:7000 reachable")),
            Err(e) => {
                failures += 1;
                line("FAIL", "reachability", &format!("cannot reach {ip}:7000 — {e}"));
            }
        }

        println!("[....] session — connecting and measuring clock-sync for 12s (streams silence, no audible output)");
        let session_result: Result<(u64, u64), String> = rt.block_on(async {
            let descriptor = cap_core::pairing::DeviceDescriptor {
                ip,
                port: 7000,
                name: format!("doctor {ip}"),
                mac: None,
                model: None,
                features: None,
            };
            let handle = tokio::time::timeout(
                Duration::from_secs(10),
                cap_core::streaming::open_live_stream(descriptor, Some(0.0), None),
            )
            .await
            .map_err(|_| "connect timed out after 10s".to_string())?
            .map_err(|e| e.to_string())?;
            let (sender, connection, _hb, sample_rate, channels) = handle.into_parts();
            // Feed real-time silence so the session behaves like a live one.
            let feeder = tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(20));
                for _ in 0..600 {
                    interval.tick().await;
                    let _ = sender.try_send(cap_core::streaming::LivePcmFrame {
                        samples: vec![0i16; 882 * channels as usize],
                        channels,
                        sample_rate,
                    });
                }
            });
            tokio::time::sleep(Duration::from_secs(12)).await;
            let timing = {
                let conn = connection.lock().await;
                conn.timing_request_count().unwrap_or(0)
            };
            feeder.abort();
            let _ = tokio::time::timeout(Duration::from_secs(3), async {
                let mut c = connection.lock().await;
                let _ = c.stop().await;
                let _ = c.disconnect().await;
            })
            .await;
            Ok((timing, 12))
        });
        match session_result {
            Ok((timing, secs)) => {
                if timing == 0 {
                    failures += 1;
                    line("FAIL", "clock-sync", &format!("0 timing requests received in {secs}s — the HomePod CANNOT reach this PC (this is the silent-stream cause). Almost always Windows Firewall: re-run 'Enable firewall access', and check the network profile above."));
                } else {
                    line("PASS", "clock-sync", &format!("{timing} timing requests answered in {secs}s — receiver can sync; audio path is healthy"));
                }
            }
            Err(e) => {
                failures += 1;
                line("FAIL", "session", &format!("could not establish a session: {e}"));
            }
        }
    } else if target.is_none() && homepods.len() <= 1 && homepods.is_empty() {
        line("SKIP", "session", "no HomePod found to test against");
    } else if target.is_none() && homepods.len() > 1 {
        line("SKIP", "session", "multiple HomePods — pick one by name to run the live test");
    }

    println!(
        "\nlogs: {}",
        crate::log_dir_hint()
    );
    if failures == 0 {
        println!("DIAGNOSIS: all checks passed. If audio is still silent, note the capture check's warning text.");
    } else {
        println!("DIAGNOSIS: {failures} failing check(s) — fix the FIRST failure above; later checks often depend on it.");
    }
    // Hard exit: the vendored session test may leave a (stoppable but slow)
    // blocking task; doctor is a short-lived tool.
    if failures == 0 {
        0
    } else {
        1
    }
}
