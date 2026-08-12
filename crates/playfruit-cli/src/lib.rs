//! Playfruit session engine: capture → AirPlay 2 pump with silence keepalive,
//! drift regulation and reconnect supervision. Used by both the CLI and the
//! tray app.

pub mod engine;

pub use engine::{Engine, EngineConfig, EngineStatus};
