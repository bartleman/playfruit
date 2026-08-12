//! Streaming session engine with reconnect supervision.
//!
//! One `Engine` = one capture device + one HomePod, supervised: if the RTSP
//! session dies (Wi-Fi blip, HomePod reboot), the engine tears down, backs
//! off, and reconnects — the capture pipeline survives across attempts.

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use audio_capture::CaptureFormat;
use cap_core::pairing::DeviceDescriptor;
use cap_core::streaming::{open_live_stream, LatencyProfile, LivePcmFrame};

/// 20 ms of silence at 44.1 kHz stereo — keepalive unit when capture is quiet.
const SILENCE_FRAMES: usize = 882;
/// ~200 ms pre-charged standing fill: fixed known latency, absorbs jitter.
const PRECHARGE_CHUNKS: usize = 10;
/// Consecutive heartbeat failures before the session is declared dead.
const HEARTBEAT_STRIKES: u32 = 3;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub ip: IpAddr,
    pub port: u16,
    pub name: String,
    pub volume: f32,
    pub latency: LatencyProfile,
}

#[derive(Debug, Clone)]
pub enum EngineStatus {
    Connecting { name: String },
    Streaming { name: String },
    Reconnecting { name: String, attempt: u32 },
    Failed(String),
    Stopped,
}

enum PumpExit {
    Stopped,
    SessionDead,
    CaptureDied,
}

pub struct Engine {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Engine {
    /// Spawns the supervisor thread. Status transitions arrive on the returned
    /// channel; the receiver may be polled (tray) or blocked on (CLI).
    pub fn start(config: EngineConfig) -> (Self, std::sync::mpsc::Receiver<EngineStatus>) {
        let (status_tx, status_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let thread = std::thread::Builder::new()
            .name("playfruit-engine".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("engine runtime");
                rt.block_on(supervise(config, stop_thread, status_tx));
            })
            .expect("engine thread");
        (
            Self {
                stop,
                thread: Some(thread),
            },
            status_rx,
        )
    }

    /// Signals the supervisor to stop and waits for clean teardown.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

async fn supervise(
    config: EngineConfig,
    stop: Arc<AtomicBool>,
    status: std::sync::mpsc::Sender<EngineStatus>,
) {
    let (capture, rx) = match audio_capture::start_loopback(CaptureFormat::AIRPLAY_DEFAULT) {
        Ok(pair) => pair,
        Err(e) => {
            let _ = status.send(EngineStatus::Failed(format!("audio capture: {e}")));
            return;
        }
    };

    let mut attempt: u32 = 0;
    let mut ever_connected = false;
    let mut backoff = Duration::from_secs(1);

    'sessions: while !stop.load(Ordering::SeqCst) {
        attempt += 1;
        let _ = status.send(if attempt == 1 {
            EngineStatus::Connecting {
                name: config.name.clone(),
            }
        } else {
            EngineStatus::Reconnecting {
                name: config.name.clone(),
                attempt,
            }
        });

        let descriptor = DeviceDescriptor {
            ip: config.ip,
            port: config.port,
            name: config.name.clone(),
            mac: None,
            model: None,
            features: None,
        };
        // Bound the connect: a black-holed IP would otherwise block for ~20 s
        // inside pairing, freezing Engine::stop (and the tray UI) with it.
        let connect = tokio::time::timeout(
            Duration::from_secs(10),
            open_live_stream(descriptor, Some(config.volume), Some(config.latency)),
        );
        let handle =
            match connect.await.unwrap_or_else(|_| {
                Err(cap_core::streaming::StreamError::Client("connect timed out".into()))
            }) {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!(error = %e, attempt, "connect failed");
                    if !ever_connected && attempt >= 3 {
                        let _ = status.send(EngineStatus::Failed(format!(
                            "could not connect to {}: {e}",
                            config.ip
                        )));
                        return;
                    }
                    // Interruptible backoff.
                    let waited = Instant::now();
                    while waited.elapsed() < backoff && !stop.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    continue 'sessions;
                }
            };
        ever_connected = true;
        let session_started = Instant::now();
        let (sender, connection, mut heartbeat, sample_rate, channels) = handle.into_parts();
        // Replace the built-in log-and-continue heartbeat with our own that
        // can actually declare the session dead.
        heartbeat.shutdown();
        drop(heartbeat);

        let _ = status.send(EngineStatus::Streaming {
            name: config.name.clone(),
        });
        tracing::info!(ip = %config.ip, "session up");

        // Discard capture frames that queued up while we were disconnected:
        // replaying them would burst stale audio into the new session and
        // permanently inflate its latency by the outage duration.
        let mut discarded = 0u32;
        while rx.try_recv().is_ok() {
            discarded += 1;
        }
        if discarded > 0 {
            tracing::info!(discarded, "dropped stale capture frames from the outage");
        }

        for _ in 0..PRECHARGE_CHUNKS {
            let _ = sender.try_send(LivePcmFrame {
                samples: vec![0i16; SILENCE_FRAMES * channels as usize],
                channels,
                sample_rate,
            });
        }

        let dead = Arc::new(AtomicBool::new(false));
        let hb_dead = dead.clone();
        let hb_conn = connection.clone();
        let heartbeat_task = tokio::spawn(async move {
            let mut strikes: u32 = 0;
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval.tick().await; // skip immediate tick
            loop {
                interval.tick().await;
                let result = { hb_conn.lock().await.send_feedback().await };
                match result {
                    Ok(()) => strikes = 0,
                    Err(e) => {
                        strikes += 1;
                        tracing::warn!(error = %e, strikes, "heartbeat failure");
                        if strikes >= HEARTBEAT_STRIKES {
                            hb_dead.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                }
            }
        });

        // Pump on the blocking pool; supervisor waits for its exit.
        let pump_rx = rx.clone();
        let pump_stop = stop.clone();
        let pump_dead = dead.clone();
        let exit = tokio::task::spawn_blocking(move || {
            pump_loop(pump_rx, sender, sample_rate, channels, pump_stop, pump_dead)
        })
        .await
        .unwrap_or(PumpExit::SessionDead);

        heartbeat_task.abort();

        // Best-effort teardown, bounded so a dead socket can't hang shutdown.
        {
            let conn = connection.clone();
            let _ = tokio::time::timeout(Duration::from_secs(3), async move {
                let mut c = conn.lock().await;
                let _ = c.stop().await;
                let _ = c.disconnect().await;
            })
            .await;
        }

        match exit {
            PumpExit::Stopped => break 'sessions,
            PumpExit::CaptureDied => {
                let _ = status.send(EngineStatus::Failed(
                    "audio capture stopped (device removed or driver error)".into(),
                ));
                return;
            }
            PumpExit::SessionDead => {
                if session_started.elapsed() > Duration::from_secs(60) {
                    backoff = Duration::from_secs(1);
                    attempt = 1; // healthy run: restart the counter for status display
                }
                tracing::warn!("session died, will reconnect");
                let waited = Instant::now();
                while waited.elapsed() < backoff && !stop.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }

    drop(capture);
    let _ = status.send(EngineStatus::Stopped);
}

/// Forwards captured PCM to the AirPlay sender with silence keepalive and
/// wall-clock drift regulation (see comments inline).
fn pump_loop(
    rx: crossbeam_channel::Receiver<audio_capture::CapturedFrame>,
    sender: cap_core::streaming::LiveFrameSender,
    sample_rate: u32,
    channels: u8,
    stop: Arc<AtomicBool>,
    dead: Arc<AtomicBool>,
) -> PumpExit {
    let ch = channels as usize;
    let start = Instant::now();
    let mut pushed_frames: u64 = 0;
    // Deadband before correcting: ±30 ms of accumulated skew.
    let deadband = (sample_rate as f64 * 0.030) as i64;
    let mut drift_dropped: u64 = 0;
    let mut drift_duped: u64 = 0;
    let mut consecutive_send_failures: u32 = 0;
    let queue_full = AtomicU64::new(0);
    let mut last_report = Instant::now();

    loop {
        if stop.load(Ordering::SeqCst) {
            return PumpExit::Stopped;
        }
        if dead.load(Ordering::SeqCst) {
            return PumpExit::SessionDead;
        }
        let mut samples = match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(frame) => frame.samples,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // Capture silent — keep the stream timeline fed in real time.
                vec![0i16; SILENCE_FRAMES * ch]
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                return PumpExit::CaptureDied
            }
        };

        // Drift regulation: hold pushed-sample rate to wall-clock 44.1 kHz by
        // shedding/duplicating ≤2 frames per chunk (≈2000 ppm authority; real
        // capture-clock drift is 20–50 ppm, so corrections stay inaudible).
        let elapsed_frames = (start.elapsed().as_secs_f64() * sample_rate as f64) as i64;
        let balance = pushed_frames as i64 - elapsed_frames;
        if balance > deadband && samples.len() >= 2 * ch {
            let shed = 2.min(samples.len() / ch - 1);
            samples.truncate(samples.len() - shed * ch);
            drift_dropped += shed as u64;
        } else if balance < -deadband && samples.len() >= ch {
            let last = samples[samples.len() - ch..].to_vec();
            for _ in 0..2 {
                samples.extend_from_slice(&last);
            }
            drift_duped += 2;
        }

        let frame_count = (samples.len() / ch) as u64;
        let ok = sender.try_send(LivePcmFrame {
            samples,
            channels,
            sample_rate,
        });
        if ok {
            // Only audio that actually entered the pipeline counts toward the
            // drift balance — counting dropped frames would erode the latency
            // cushion a little more on every drop.
            pushed_frames += frame_count;
            consecutive_send_failures = 0;
        } else {
            consecutive_send_failures += 1;
            // The sender channel stays closed once the vendored streamer task
            // dies; without this check the engine would report "Streaming"
            // over a silent session forever. ~5 s of solid failures = dead.
            if consecutive_send_failures >= 250 {
                tracing::warn!("sender unresponsive for ~5s — treating session as dead");
                return PumpExit::SessionDead;
            }
            let n = queue_full.fetch_add(1, Ordering::Relaxed) + 1;
            if n % 250 == 1 {
                tracing::warn!(total = n, "sender queue full, dropping capture frames");
            }
        }

        if last_report.elapsed() > Duration::from_secs(600) {
            tracing::info!(
                drift_dropped,
                drift_duped,
                minutes = start.elapsed().as_secs() / 60,
                "drift regulation stats"
            );
            last_report = Instant::now();
        }
    }
}
