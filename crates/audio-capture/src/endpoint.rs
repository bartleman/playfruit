//! Default-render-endpoint mute control for "HomePod only" mode.
//!
//! Caveat that shapes the whole feature: on many drivers the WASAPI loopback
//! tap sits AFTER the endpoint volume/mute stage, so muting the speakers also
//! silences our capture. Callers must therefore verify capture keeps flowing
//! after muting and roll back if it doesn't (the engine's health task does).

use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

fn endpoint_volume() -> Result<IAudioEndpointVolume, String> {
    // Idempotent per-thread COM init (same MTA the capture uses).
    let _ = wasapi::initialize_mta();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| format!("device enumerator: {e}"))?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| format!("default endpoint: {e}"))?;
        device
            .Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
            .map_err(|e| format!("endpoint volume: {e}"))
    }
}

/// Mute/unmute the default output device. Returns the PREVIOUS mute state so
/// callers can restore it.
pub fn set_output_mute(mute: bool) -> Result<bool, String> {
    let vol = endpoint_volume()?;
    unsafe {
        let prev = vol.GetMute().map_err(|e| e.to_string())?.as_bool();
        vol.SetMute(mute, std::ptr::null())
            .map_err(|e| e.to_string())?;
        Ok(prev)
    }
}
