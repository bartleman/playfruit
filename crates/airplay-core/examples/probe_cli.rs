//! Probe rápido por línea de comandos:  cargo run -p airplay-core --example probe_cli -- 192.168.1.50

use std::net::IpAddr;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let ip: IpAddr = args
        .get(1)
        .expect("uso: probe_cli <ip> [port]")
        .parse()
        .expect("IP inválida");
    let port: u16 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(7000);

    match cap_core::probe_airplay(ip, port).await {
        Ok(result) => {
            println!("OK ({} → {})", ip, port);
            if let Some(s) = result.server_header {
                println!("Server: {s}");
            }
            println!("--- respuesta ---");
            println!("{}", result.raw_response.trim_end());
        }
        Err(e) => {
            eprintln!("FALLO: {e}");
            std::process::exit(1);
        }
    }
}
