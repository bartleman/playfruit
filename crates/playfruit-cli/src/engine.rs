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
/// ~300 ms pre-charged standing fill: fixed known latency, absorbs jitter.
/// (200 ms proved too tight for Windows desktop scheduling.)
const PRECHARGE_CHUNKS: usize = 15;
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
    /// Connected and healthy, but the capture source has produced no real
    /// (non-zero) audio for a few seconds — usually "nothing is playing on
    /// this PC", the most common no-sound cause and user-fixable.
    StreamingSilent { name: String },
    /// Session is up but something needs user attention (e.g. the receiver's
    /// clock-sync queries never arrive — firewall blocking inbound UDP).
    Warning { name: String, message: String },
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
                // Abandon instead of dropping: Runtime::drop waits forever for
                // spawn_blocking tasks, and the vendored control loop is one
                // (now stoppable via our vendor patch, but shutdown_background
                // keeps stop() prompt even if a future session leaves one
                // behind). Leaked worst case: one near-idle thread per session.
                rt.shutdown_background();
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

    /// Signals stop and reaps the engine thread on a background thread, so
    /// callers on UI threads never block on teardown (worst case ~3s of
    /// session teardown happens out of sight).
    pub fn stop_detached(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = std::thread::Builder::new()
                .name("playfruit-engine-reaper".into())
                .spawn(move || {
                    let _ = t.join();
                });
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
        // Also poll the stop flag so Engine::stop stays prompt mid-connect.
        let connect = tokio::time::timeout(
            Duration::from_secs(10),
            open_live_stream(descriptor, Some(config.volume), Some(config.latency)),
        );
        let stop_poll = stop.clone();
        let connect_result = tokio::select! {
            r = connect => r.unwrap_or_else(|_| {
                Err(cap_core::streaming::StreamError::Client("connect timed out".into()))
            }),
            _ = async move {
                loop {
                    if stop_poll.load(Ordering::SeqCst) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            } => break 'sessions,
        };
        let handle =
            match connect_result {
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
        // Milliseconds since session epoch of the last NON-ZERO capture frame;
        // u64::MAX = none yet. Written by the pump, read by the health task.
        let last_audio_ms = Arc::new(AtomicU64::new(u64::MAX));
        let session_epoch = Instant::now();
        let hb_dead = dead.clone();
        let hb_conn = connection.clone();
        let hb_status = status.clone();
        let hb_name = config.name.clone();
        let hb_last_audio = last_audio_ms.clone();
        let heartbeat_task = tokio::spawn(async move {
            let mut strikes: u32 = 0;
            let mut silent_reported = false;
            let mut timing_warned = false;
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval.tick().await; // skip immediate tick
            loop {
                interval.tick().await;
                let (result, timing_count) = {
                    let mut conn = hb_conn.lock().await;
                    (conn.send_feedback().await, conn.timing_request_count())
                };
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

                let age = session_epoch.elapsed();

                // Firewall tell: the HomePod queries our clock several times a
                // second when it can reach us. Zero queries after 10s means
                // inbound UDP is blocked and the speaker will play silence.
                if !timing_warned && age > Duration::from_secs(10) && timing_count == Some(0) {
                    timing_warned = true;
                    tracing::warn!(
                        "no inbound timing requests after 10s — Windows Firewall is \
                         likely blocking UDP (check firewall access + network profile)"
                    );
                    let _ = hb_status.send(EngineStatus::Warning {
                        name: hb_name.clone(),
                        message: "HomePod can't sync its clock — audio will stay silent. \
                                  Re-run 'Enable firewall access' in the menu."
                            .into(),
                    });
                }

                // Silence tell: connected but the PC isn't producing audio —
                // normal (nothing playing), so it's a state, not an error.
                if age > Duration::from_secs(5) && !timing_warned {
                    let last = hb_last_audio.load(Ordering::Relaxed);
                    let real_recent = last != u64::MAX
                        && age.as_millis() as u64 - last < 3_000;
                    if !real_recent && !silent_reported {
                        silent_reported = true;
                        let _ = hb_status.send(EngineStatus::StreamingSilent {
                            name: hb_name.clone(),
                        });
                    } else if real_recent && silent_reported {
                        silent_reported = false;
                        let _ = hb_status.send(EngineStatus::Streaming {
                            name: hb_name.clone(),
                        });
                    }
                }
            }
        });

        // Pump on the blocking pool; supervisor waits for its exit.
        let pump_rx = rx.clone();
        let pump_stop = stop.clone();
        let pump_dead = dead.clone();
        let pump_last_audio = last_audio_ms.clone();
        let exit = tokio::task::spawn_blocking(move || {
            pump_loop(
                pump_rx,
                sender,
                sample_rate,
                channels,
                pump_stop,
                pump_dead,
                pump_last_audio,
                session_epoch,
            )
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
    last_audio_ms: Arc<AtomicU64>,
    session_epoch: Instant,
) -> PumpExit {
    let ch = channels as usize;
    let start = Instant::now();
    let mut pushed_frames: u64 = 0;
    // Deadband before correcting: ±30 ms of accumulated skew.
    let deadband = (sample_rate as f64 * 0.030) as i64;
    let mut drift_dropped: u64 = 0;
    let mut drift_duped: u64 = 0;
    let mut keepalive_frames: u64 = 0;
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
            Ok(frame) => {
                // Real (non-digital-silence) audio powers the StreamingSilent
                // state machine in the health task.
                if frame.samples.iter().any(|&s| s != 0) {
                    last_audio_ms.store(
                        session_epoch.elapsed().as_millis() as u64,
                        Ordering::Relaxed,
                    );
                }
                frame.samples
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // Deficit-gated keepalive. A timeout alone means nothing on a
                // desktop OS — normal threads routinely stall 20-50ms and the
                // queued real frames arrive right after; injecting fixed
                // silence on every timeout fragments the audio and pushes the
                // stream ahead of real time until the send queue chokes
                // (field symptom: plays a moment, dies, repeats). Only inject
                // when the stream is genuinely BEHIND wall-clock real time —
                // true capture silence — and inject the actual deficit.
                let elapsed_frames =
                    (start.elapsed().as_secs_f64() * sample_rate as f64) as i64;
                let deficit = elapsed_frames - pushed_frames as i64;
                if deficit <= deadband {
                    continue;
                }
                // Cap per-iteration injection at 200ms to catch up smoothly.
                let inject = (deficit as usize).min(sample_rate as usize / 5);
                keepalive_frames += inject as u64;
                if keepalive_frames == inject as u64 {
                    tracing::info!("capture silent — timeline keepalive engaged");
                }
                vec![0i16; inject * ch]
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
                keepalive_frames,
                queue_full = queue_full.load(Ordering::Relaxed),
                minutes = start.elapsed().as_secs() / 60,
                "pump stats"
            );
            last_report = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cap_core::streaming::LiveAudioDecoder;

    fn test_channel() -> (
        crossbeam_channel::Sender<audio_capture::CapturedFrame>,
        crossbeam_channel::Receiver<audio_capture::CapturedFrame>,
    ) {
        crossbeam_channel::bounded(64)
    }

    /// Regression: the tray froze because stopping never returned. The pump
    /// must exit promptly when the stop flag is raised, even with a silent
    /// capture source.
    #[test]
    fn pump_stop_is_prompt() {
        let (_tx, rx) = test_channel();
        let (sender, _decoder) = LiveAudioDecoder::create_pair(44_100, 2, 64);
        let stop = Arc::new(AtomicBool::new(false));
        let dead = Arc::new(AtomicBool::new(false));
        let last_audio = Arc::new(AtomicU64::new(u64::MAX));
        let epoch = Instant::now();

        let stop_setter = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            stop_setter.store(true, Ordering::SeqCst);
        });

        let started = Instant::now();
        let exit = pump_loop(rx, sender, 44_100, 2, stop, dead, last_audio, epoch);
        assert!(matches!(exit, PumpExit::Stopped));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "pump took {:?} to stop",
            started.elapsed()
        );
    }

    /// Regression: a dead vendored streamer (closed frame channel) must be
    /// detected as SessionDead instead of reporting Streaming forever.
    #[test]
    fn pump_detects_unresponsive_sender() {
        let (_tx, rx) = test_channel();
        // Tiny queue with no consumer: try_send fails permanently once full,
        // mimicking a dead streamer task.
        let (sender, _decoder) = LiveAudioDecoder::create_pair(44_100, 2, 1);
        let stop = Arc::new(AtomicBool::new(false));
        let dead = Arc::new(AtomicBool::new(false));
        let last_audio = Arc::new(AtomicU64::new(u64::MAX));

        let started = Instant::now();
        let exit = pump_loop(
            rx,
            sender,
            44_100,
            2,
            stop,
            dead,
            last_audio,
            Instant::now(),
        );
        assert!(matches!(exit, PumpExit::SessionDead));
        // 250 consecutive failures at ~20ms keepalive cadence ≈ 5s nominal;
        // generous margin because shared CI runners stretch timer waits.
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "dead-sender detection took {:?}",
            started.elapsed()
        );
    }

    /// Regression: capture channel disconnect must surface as CaptureDied.
    #[test]
    fn pump_reports_capture_death() {
        let (tx, rx) = test_channel();
        let (sender, _decoder) = LiveAudioDecoder::create_pair(44_100, 2, 64);
        let stop = Arc::new(AtomicBool::new(false));
        let dead = Arc::new(AtomicBool::new(false));
        drop(tx);
        let exit = pump_loop(
            rx,
            sender,
            44_100,
            2,
            stop,
            dead,
            Arc::new(AtomicU64::new(u64::MAX)),
            Instant::now(),
        );
        assert!(matches!(exit, PumpExit::CaptureDied));
    }

    /// Regression: Engine::stop must return promptly in every phase, even
    /// mid-connect against a black-holed address (192.0.2.1, TEST-NET) and
    /// regardless of whether the capture layer initialized. This is THE
    /// tray-freeze test.
    #[test]
    fn engine_stop_is_prompt_during_connect() {
        let (engine, _status) = Engine::start(EngineConfig {
            ip: "192.0.2.1".parse().unwrap(),
            port: 7000,
            name: "blackhole".into(),
            volume: 0.1,
            latency: LatencyProfile::Gaming,
        });
        std::thread::sleep(Duration::from_millis(300));
        let started = Instant::now();
        engine.stop();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "Engine::stop took {:?}",
            started.elapsed()
        );
    }
}
