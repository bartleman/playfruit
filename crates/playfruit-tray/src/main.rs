//! Playfruit tray app: pick a HomePod from the tray menu, stream system audio
//! to it with ~half-second latency. Windows-first; also builds on macOS/Linux
//! for development.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use cap_core::discovery::{browse_once, DeviceKind};
use cap_core::streaming::LatencyProfile;
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use playfruit_cli::{Engine, EngineConfig, EngineStatus};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Config {
    volume: f32,
    latency: LatencyProfile,
    last_ip: Option<String>,
    last_name: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            volume: 0.5,
            latency: LatencyProfile::Gaming,
            last_ip: None,
            last_name: None,
        }
    }
}

fn config_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
        })
        .unwrap_or_else(std::env::temp_dir);
    base.join("playfruit")
}

fn load_config() -> Config {
    std::fs::read(config_dir().join("config.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_config(cfg: &Config) {
    let dir = config_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(bytes) = serde_json::to_vec_pretty(cfg) {
        let _ = std::fs::write(dir.join("config.json"), bytes);
    }
}

/// Simple generated icon: filled circle, gray when idle, green when streaming.
fn make_icon(streaming: bool) -> Icon {
    const S: i32 = 32;
    let (r, g, b) = if streaming {
        (52u8, 199u8, 89u8)
    } else {
        (142u8, 142u8, 147u8)
    };
    let mut rgba = Vec::with_capacity((S * S * 4) as usize);
    for y in 0..S {
        for x in 0..S {
            let dx = (x - S / 2) as f32 + 0.5;
            let dy = (y - S / 2) as f32 + 0.5;
            let d = (dx * dx + dy * dy).sqrt();
            if d <= 13.0 {
                // ring highlight for a bit of depth
                if d >= 11.0 {
                    rgba.extend_from_slice(&[255, 255, 255, 255]);
                } else {
                    rgba.extend_from_slice(&[r, g, b, 255]);
                }
            } else if d <= 14.0 {
                let a = ((14.0 - d) * 255.0) as u8; // antialias edge
                rgba.extend_from_slice(&[255, 255, 255, a]);
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    Icon::from_rgba(rgba, S as u32, S as u32).expect("icon")
}

#[derive(Debug, Clone)]
struct Speaker {
    name: String,
    ip: IpAddr,
    port: u16,
    kind: DeviceKind,
}

fn scan_speakers(rt: &tokio::runtime::Runtime) -> Vec<Speaker> {
    let devices = rt
        .block_on(browse_once(Duration::from_secs(2)))
        .unwrap_or_default();
    let mut out: Vec<Speaker> = Vec::new();
    for d in devices {
        // RAOP entries repeat the AirPlay ones under "MAC@Name"; skip them.
        if d.name.contains('@') {
            continue;
        }
        if !d.supports_airplay2 {
            continue;
        }
        let Some(ip) = d
            .addresses
            .iter()
            .find(|a| a.is_ipv4())
            .or_else(|| d.addresses.first())
            .copied()
        else {
            continue;
        };
        // Skip other computers advertising AirPlay-receiver services.
        if !matches!(d.kind, DeviceKind::HomePod | DeviceKind::AppleTv) {
            continue;
        }
        if out.iter().any(|s| s.ip == ip) {
            continue;
        }
        out.push(Speaker {
            name: d.name,
            ip,
            port: 7000,
            kind: d.kind,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn speaker_label(s: &Speaker) -> String {
    match s.kind {
        DeviceKind::HomePod => format!("{}  (HomePod)", s.name),
        DeviceKind::AppleTv => format!("{}  (Apple TV)", s.name),
        _ => s.name.clone(),
    }
}

struct App {
    engine: Option<Engine>,
    status_rx: Option<Receiver<EngineStatus>>,
    current: Option<Speaker>,
    config: Config,
    // menu handles
    tray: TrayIcon,
    status_item: MenuItem,
    devices_menu: Submenu,
    device_items: Vec<(CheckMenuItem, Speaker)>,
    latency_items: Vec<(CheckMenuItem, LatencyProfile)>,
    volume_items: Vec<(CheckMenuItem, f32)>,
    disconnect_item: MenuItem,
}

impl App {
    fn set_status_text(&self, text: &str, streaming: bool) {
        self.status_item.set_text(format!("Playfruit — {text}"));
        let _ = self.tray.set_tooltip(Some(format!("Playfruit — {text}")));
        let _ = self.tray.set_icon(Some(make_icon(streaming)));
    }

    fn refresh_checks(&self) {
        for (item, sp) in &self.device_items {
            item.set_checked(
                self.current.as_ref().map(|c| c.ip) == Some(sp.ip) && self.engine.is_some(),
            );
        }
        for (item, p) in &self.latency_items {
            item.set_checked(*p == self.config.latency);
        }
        for (item, v) in &self.volume_items {
            item.set_checked((v - self.config.volume).abs() < 0.01);
        }
        self.disconnect_item.set_enabled(self.engine.is_some());
    }

    fn connect(&mut self, sp: Speaker) {
        if let Some(e) = self.engine.take() {
            e.stop();
        }
        let (engine, rx) = Engine::start(EngineConfig {
            ip: sp.ip,
            port: sp.port,
            name: sp.name.clone(),
            volume: self.config.volume,
            latency: self.config.latency,
        });
        self.engine = Some(engine);
        self.status_rx = Some(rx);
        self.config.last_ip = Some(sp.ip.to_string());
        self.config.last_name = Some(sp.name.clone());
        self.current = Some(sp);
        save_config(&self.config);
        self.refresh_checks();
    }

    fn disconnect(&mut self) {
        if let Some(e) = self.engine.take() {
            e.stop();
        }
        self.status_rx = None;
        self.set_status_text("idle", false);
        self.refresh_checks();
    }

    /// Restart the active session (after a latency/volume change).
    fn restart_if_active(&mut self) {
        if self.engine.is_some() {
            if let Some(sp) = self.current.clone() {
                self.connect(sp);
            }
        }
    }

    fn rebuild_device_items(&mut self, speakers: Vec<Speaker>) {
        for (item, _) in &self.device_items {
            let _ = self.devices_menu.remove(item);
        }
        self.device_items.clear();
        if speakers.is_empty() {
            let placeholder = CheckMenuItem::with_id(
                "dev:none",
                "No AirPlay 2 devices found",
                false,
                false,
                None,
            );
            let _ = self.devices_menu.append(&placeholder);
            self.device_items.push((
                placeholder,
                Speaker {
                    name: String::new(),
                    ip: IpAddr::from([0, 0, 0, 0]),
                    port: 0,
                    kind: DeviceKind::OtherAirPlay,
                },
            ));
            return;
        }
        for (i, sp) in speakers.into_iter().enumerate() {
            let item = CheckMenuItem::with_id(
                format!("dev:{i}"),
                speaker_label(&sp),
                true,
                false,
                None,
            );
            let _ = self.devices_menu.append(&item);
            self.device_items.push((item, sp));
        }
        self.refresh_checks();
    }
}

fn main() {
    // Log to a file: the windowed subsystem has no console.
    let log_dir = config_dir();
    let _ = std::fs::create_dir_all(&log_dir);
    if let Ok(file) = std::fs::File::create(log_dir.join("playfruit-tray.log")) {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .try_init();
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");

    let config = load_config();

    // --- menu ---
    let menu = Menu::new();
    let status_item = MenuItem::with_id("status", "Playfruit — idle", false, None);
    let devices_menu = Submenu::new("Stream to", true);
    let rescan_item = MenuItem::with_id("rescan", "Rescan devices", true, None);

    let latency_menu = Submenu::new("Latency", true);
    let latency_items: Vec<(CheckMenuItem, LatencyProfile)> = [
        ("lat:gaming", "Gaming (~0.5 s, best for sports)", LatencyProfile::Gaming),
        ("lat:video", "Video (~0.6 s, steadier)", LatencyProfile::Video),
        ("lat:music", "Music (~0.7 s, most robust)", LatencyProfile::Music),
    ]
    .into_iter()
    .map(|(id, label, p)| {
        let item = CheckMenuItem::with_id(id, label, true, false, None);
        let _ = latency_menu.append(&item);
        (item, p)
    })
    .collect();

    let volume_menu = Submenu::new("Volume", true);
    let volume_items: Vec<(CheckMenuItem, f32)> = [
        ("vol:20", "20%", 0.20f32),
        ("vol:35", "35%", 0.35),
        ("vol:50", "50%", 0.50),
        ("vol:75", "75%", 0.75),
        ("vol:100", "100%", 1.0),
    ]
    .into_iter()
    .map(|(id, label, v)| {
        let item = CheckMenuItem::with_id(id, label, true, false, None);
        let _ = volume_menu.append(&item);
        (item, v)
    })
    .collect();

    let disconnect_item = MenuItem::with_id("disconnect", "Disconnect", false, None);
    let quit_item = MenuItem::with_id("quit", "Quit Playfruit", true, None);

    let _ = menu.append_items(&[
        &status_item,
        &PredefinedMenuItem::separator(),
        &devices_menu,
        &rescan_item,
        &PredefinedMenuItem::separator(),
        &latency_menu,
        &volume_menu,
        &PredefinedMenuItem::separator(),
        &disconnect_item,
        &quit_item,
    ]);

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Playfruit — idle")
        .with_icon(make_icon(false))
        .build()
        .expect("tray icon");

    let mut app = App {
        engine: None,
        status_rx: None,
        current: None,
        config,
        tray,
        status_item,
        devices_menu,
        device_items: Vec::new(),
        latency_items,
        volume_items,
        disconnect_item,
    };

    // Initial device scan.
    let speakers = scan_speakers(&rt);
    app.rebuild_device_items(speakers);
    app.refresh_checks();

    let menu_rx = MenuEvent::receiver();
    let event_loop = EventLoopBuilder::new().build();
    event_loop.run(move |_event, _target, control_flow| {
        *control_flow =
            ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(150));

        while let Ok(ev) = menu_rx.try_recv() {
            let id = ev.id().0.as_str().to_string();
            match id.as_str() {
                "quit" => {
                    if let Some(e) = app.engine.take() {
                        e.stop();
                    }
                    // Hard exit: vendored retransmit loop has no shutdown.
                    std::process::exit(0);
                }
                "disconnect" => app.disconnect(),
                "rescan" => {
                    let speakers = scan_speakers(&rt);
                    app.rebuild_device_items(speakers);
                }
                other => {
                    if let Some(idx) = other.strip_prefix("dev:").and_then(|s| s.parse::<usize>().ok()) {
                        if let Some((_, sp)) = app.device_items.get(idx) {
                            if !sp.name.is_empty() {
                                let sp = sp.clone();
                                app.connect(sp);
                            }
                        }
                    } else if let Some(p) = app
                        .latency_items
                        .iter()
                        .find(|(item, _)| item.id().0 == other)
                        .map(|(_, p)| *p)
                    {
                        app.config.latency = p;
                        save_config(&app.config);
                        app.refresh_checks();
                        app.restart_if_active();
                    } else if let Some(v) = app
                        .volume_items
                        .iter()
                        .find(|(item, _)| item.id().0 == other)
                        .map(|(_, v)| *v)
                    {
                        app.config.volume = v;
                        save_config(&app.config);
                        app.refresh_checks();
                        app.restart_if_active();
                    }
                }
            }
        }

        // Engine status → tray UI.
        let mut status_update: Option<(String, bool, bool)> = None; // (text, streaming, clear_engine)
        if let Some(rx) = &app.status_rx {
            while let Ok(st) = rx.try_recv() {
                status_update = Some(match st {
                    EngineStatus::Connecting { name } => (format!("connecting to {name}…"), false, false),
                    EngineStatus::Streaming { name } => (format!("streaming to {name}"), true, false),
                    EngineStatus::Reconnecting { name, attempt } => {
                        (format!("reconnecting to {name} (attempt {attempt})…"), false, false)
                    }
                    EngineStatus::Failed(e) => (format!("failed: {e}"), false, true),
                    EngineStatus::Stopped => ("idle".to_string(), false, true),
                });
            }
        }
        if let Some((text, streaming, clear)) = status_update {
            if clear {
                if let Some(e) = app.engine.take() {
                    e.stop();
                }
                app.status_rx = None;
            }
            app.set_status_text(&text, streaming);
            app.refresh_checks();
        }
    });
}
