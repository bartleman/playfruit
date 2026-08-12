//! Prueba de latencia audible: manda un click de 60 ms cada segundo al HomePod
//! y a la vez reproduce un click local en el Mac (afplay). El desfase entre
//! ambos clicks ES la latencia extremo a extremo (± ~150 ms de arranque de
//! afplay).
//!
//!   cargo run -p cap-core --release --example click_cli -- <ip> [clicks=12] [vol=0.3]

use std::net::IpAddr;
use std::time::Duration;

use cap_core::pairing::DeviceDescriptor;
use cap_core::streaming::{open_live_stream, LatencyProfile};

const SAMPLE_RATE: u32 = 44_100;
const CHANNELS: usize = 2;
/// 20 ms por chunk: 882 frames.
const CHUNK_FRAMES: usize = 882;
const CHUNKS_PER_SEC: usize = 50;
/// Click = primeros 3 chunks de cada segundo (60 ms de seno 1 kHz con rampa).
const CLICK_CHUNKS: usize = 3;
/// Precarga de silencio para dejar un fill estable de ~200 ms en el buffer
/// del sender (latencia fija conocida, evita underruns por jitter).
const PRECHARGE_MS: usize = 200;

fn make_chunk(click_phase: Option<usize>) -> Vec<i16> {
    let mut samples = vec![0i16; CHUNK_FRAMES * CHANNELS];
    if let Some(phase_chunk) = click_phase {
        let total = (CLICK_CHUNKS * CHUNK_FRAMES) as f32;
        for f in 0..CHUNK_FRAMES {
            let n = (phase_chunk * CHUNK_FRAMES + f) as f32;
            // Rampa triangular para evitar pops.
            let env = 1.0 - (2.0 * n / total - 1.0).abs();
            let s = (n * 1000.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin();
            let v = (s * env * 0.5 * i16::MAX as f32) as i16;
            samples[f * 2] = v;
            samples[f * 2 + 1] = v;
        }
    }
    samples
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let ip: IpAddr = args
        .get(1)
        .expect("uso: click_cli <ip> [clicks=12] [vol=0.3]")
        .parse()
        .expect("IP inválida");
    let clicks: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(12);
    let vol: f32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0.3);

    let desc = DeviceDescriptor {
        ip,
        port: 7000,
        name: format!("HomePod {ip}"),
        mac: None,
        model: None,
        features: None,
    };

    println!("Abriendo stream (perfil Gaming, ~250 ms receptor)...");
    let handle = match open_live_stream(desc, Some(vol), Some(LatencyProfile::Gaming)).await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("✗ no se pudo abrir stream: {e}");
            std::process::exit(1);
        }
    };

    // Precarga: fija el standing fill del sender (~200 ms de latencia conocida).
    let precharge_chunks = PRECHARGE_MS / 20;
    for _ in 0..precharge_chunks {
        handle.push_pcm(make_chunk(None));
    }

    println!("== {clicks} clicks, 1/s. Compara el click del Mac con el del HomePod ==");

    let mut interval = tokio::time::interval(Duration::from_millis(20));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    let total_chunks = clicks * CHUNKS_PER_SEC + CHUNKS_PER_SEC; // +1 s de cola
    for i in 0..total_chunks {
        interval.tick().await;
        let pos = i % CHUNKS_PER_SEC;
        let click_no = i / CHUNKS_PER_SEC;
        let chunk = if pos < CLICK_CHUNKS && click_no < clicks {
            if pos == 0 {
                // Referencia local, disparada al empujar el click.
                #[cfg(target_os = "macos")]
                let _ = std::process::Command::new("afplay")
                    .arg("/System/Library/Sounds/Pop.aiff")
                    .spawn();
                println!("CLICK {}", click_no + 1);
            }
            make_chunk(Some(pos))
        } else {
            make_chunk(None)
        };
        if !handle.push_pcm(chunk) {
            eprintln!("(cola llena, chunk descartado)");
        }
    }

    println!("✓ Test completado.");
    // Salida dura: el retransmit loop del fork no tiene shutdown y colgaría el proceso.
    std::process::exit(0);
}
