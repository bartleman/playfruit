# Playfruit 🍐

Stream your PC's system audio to a HomePod over AirPlay 2 with ~half-second
latency — low enough to watch live sports with the video on screen.

Born from a specific itch: watching football on a Windows machine with the
sound on a HomePod, without the ~2 s delay that standard AirPlay senders
impose.

> **Expectations**: this is a personal project, maintained best-effort.
> AirPlay 2 is reverse-engineered; Apple firmware updates can (and
> occasionally do) break senders like this one until the community catches
> up. Issues and PRs welcome — see [CONTRIBUTING.md](CONTRIBUTING.md).

## Status / maturity

- ✅ AirPlay 2 transient pairing + encrypted realtime streaming, validated
  against a HomePod mini and a HomePod gen 2 (firmware 26.6)
- ✅ ~450–500 ms end-to-end latency (informal ear-measurement with the
  bundled `click_cli` test tool)
- ✅ Clock-drift regulation for multi-hour streams, silence keepalive
  (quiet periods don't kill the session), auto-reconnect with backoff
- ✅ **Field-validated on real Windows hardware** (v0.1.7+): system audio
  streaming to a HomePod with the echo-free "HomePod only" mode engaging
  and restoring correctly. Three field-test cycles hardened the firewall
  flow, the capture layer, and the session engine (each failure became a
  regression test).
- ⚠️ Releases are still marked pre-release while profile tuning settles:
  on busy Wi-Fi the aggressive profiles can "robotize" (the speaker repeats
  a packet when its buffer runs dry) — if you hear it, switch **Latency**
  to `Music`; the app also warns when it measures the network dropping
  audio. Ethernet on the PC helps more than any setting.

## Install

**Windows (recommended):** download `playfruit-windows-*.zip` from
[Releases](https://github.com/bartleman/playfruit/releases), unzip, and run
`playfruit-tray.exe`. No installer, no dependencies.

First run on Windows:

- **SmartScreen** warns because the binaries are unsigned: *More info →
  Run anyway*.
- **Firewall**: the administrator prompt appears automatically on first
  run — one approval adds a program-scoped inbound rule covering ALL
  network profiles, before the app touches the network (so Windows' own
  firewall dialog never appears). Required because AirPlay receivers send
  timing/sync packets *back* to the app over UDP; without it the HomePod
  connects but plays silence. Declined it? The tray menu's "Enable firewall
  access…" retries anytime; `--remove-firewall` cleans up on uninstall.

## Using the tray app

Run `playfruit-tray.exe`. A gray circle appears in the system tray (check
the `^` overflow area near the clock). Right-click it:

- **Mirror PC audio to** — HomePods and Apple TVs discovered on your
  network; click one to connect. Playfruit mirrors whatever your PC's
  default output plays (it does not appear as a separate playback device).
- **Latency** — `Video` (~0.6 s, default), `Gaming` (~0.5 s, for strong
  Wi-Fi), `Music` (~0.7 s, most robust). If audio stutters or "robotizes",
  step toward `Music`.
- **HomePod only (mute PC speakers)** — on by default: once audio is
  flowing, the PC's speakers mute so you hear only the HomePod (no ~0.6s
  echo). Speakers restore automatically on disconnect/quit. On the minority
  of audio drivers where muting the output also silences the capture,
  Playfruit detects it within seconds, restores your speakers, and tells
  you — route audio to an unused output device in that case.
- **Volume** presets, **Disconnect**, **Quit**.

Settings persist in `%APPDATA%\playfruit\config.json`; logs are at
`%APPDATA%\playfruit\playfruit-tray.log`.

## Using the CLI

```
playfruit <name-or-ip> [--volume 0.5] [--latency gaming|video|music] [--keep-pc-audio]
```

The PC's speakers mute while streaming (no echo); `--keep-pc-audio` opts out.

Device names work directly — `playfruit kitchen` finds the speaker via
mDNS (case-insensitive fragment match). `playfruit doctor` lists everything
it can see, with addresses.

Test tools (in `crates/airplay-core/examples/`):

- `click_cli <ip> [clicks] [vol]` — 1/s click track for ear-measuring
  end-to-end latency (on macOS it also plays a local reference click)
- `tone_cli <ip> [secs] [hz] [amp]` — stream a sine tone
- `probe_cli <ip>` / `pair_cli <ip>` — silent protocol/pairing checks

## Diagnosing problems

`playfruit doctor` (optionally with a device name or IP) checks the whole pipeline in ~30 seconds:
firewall rules (including whether they cover your active network profile),
audio capture (frames/sec and whether real audio is flowing), device
discovery, reachability, and — the decisive one — whether the HomePod's
clock-sync queries actually reach this PC (if they don't, the stream is
silent; that's the most common failure). See [FIELD_TEST.md](FIELD_TEST.md)
for the full 10-minute test protocol.

The tray icon tells the story at a glance: gray idle, yellow connecting or
reconnecting, **green streaming**, *pale green* connected-but-the-PC-is-silent
("nothing is playing"), red stopped-on-error (status line names the cause).

## Troubleshooting

- **Silence but the app says streaming** — run `playfruit doctor <ip>`;
  a failing `clock-sync` check means the firewall is blocking the HomePod's
  timing queries: re-run "Enable firewall access…" (it upgrades old rules
  to cover Public-profile networks).
- **Stutter / robotized audio** — Wi-Fi congestion; switch the latency
  profile to `Video` or `Music`, prefer Ethernet on the PC, or move the
  HomePod closer to the access point.
- **Wi-Fi blip mid-match** — the engine detects the dead session within ~6 s
  and reconnects automatically with backoff; the tray tooltip shows progress.
- **Linux** — capture uses the PulseAudio/PipeWire monitor via `parec`
  (install `pulseaudio-utils` or `pipewire-pulse`); falls back to the `cpal`
  input device. Linux is a development platform, not a supported target yet.

## Building from source

Prerequisites: [Rust](https://rustup.rs) stable.

```sh
# Native build (Windows, macOS, Linux)
cargo build --release -p playfruit-cli -p playfruit-tray

# Cross-compile Windows binaries from macOS/Linux (needs mingw-w64)
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu -p playfruit-cli -p playfruit-tray
```

Binaries land in `target/release/` (`playfruit`, `playfruit-tray`).

## Provenance & license

Licensed **GPL-2.0-or-later**. The upstream projects below declare GPL-2.0
without specifying "only" or "or later" in their grant; per GPLv2 §9 this
project distributes under "any version" terms and declares or-later (which
also resolves Apache-2.0 dependency compatibility via GPLv3). If an upstream
states GPL-2.0-only, this declaration will be revisited.

The AirPlay 2 protocol stack is vendored from
[Pabldi08/airplay2-rs](https://github.com/Pabldi08/airplay2-rs) @ `1baeaae`
(a fork of [lmcgartland/airplay2-rs](https://github.com/lmcgartland/airplay2-rs)).
The capture and connection-glue crates originate from
[Pabldi08/AirSend](https://github.com/Pabldi08/AirSend) (GPL-2.0 per its
`Cargo.toml` at the time of import, 2026-08). Thanks to both authors — the
hard protocol work is theirs.

**Local modifications to the vendored tree** (each patched file carries a
`MODIFIED` notice):

- `airplay-audio/src/streamer.rs` — live-mode prefill wait cut from 5 s
  (which always timed out at 0% for live sources) to 250 ms, with the
  caller pre-charging a real ~300 ms standing fill the prefill can see;
  the live channel drains eagerly up to a 95% buffer gate (the old 40%
  threshold hid ~½ s of channel backlog as silent latency; 95% keeps
  overflow structurally impossible — a full-buffer push is fatal upstream);
  underrun re-buffer threshold 10% → 2.5% (sized to the smaller standing
  fill); per-packet jitter/decode warnings demoted to debug (each was a
  synchronous file write inside the packet-pacing loop).
- `airplay-client/src/connection.rs` — `send_feedback()` propagates timeouts
  as errors instead of masking them (dead sessions used to look healthy
  forever); the retransmit control loop gets a cooperative shutdown flag
  (it previously ran forever in a blocking task and hung tokio runtime
  shutdown — frozen callers); `timing_request_count()` and
  `rtx_requested()` accessors for health monitoring.
- `airplay-timing/src/ntp.rs` — counts inbound timing requests so the app
  can detect a receiver that cannot reach us (firewall) instead of playing
  silence with no explanation.
- `airplay-audio/Cargo.toml`, `src/encoder.rs`, `src/lib.rs` — the AAC
  encoder is feature-gated off by default (`fdk-aac`'s license is
  GPL-incompatible and it's unnecessary: streaming uses ALAC).

Several of these fix upstream defects (the feedback-timeout masking, the
unstoppable control loop, the dead prefill wait, hot-loop logging, the
fdk-aac licensing issue) and are offered back upstream — see
[Pabldi08/AirSend#5](https://github.com/Pabldi08/AirSend/issues/5).

Comments in inherited code are partly in Spanish (upstream's language);
they're welcome to stay, and translation PRs are equally welcome.

*Playfruit is an independent project, not affiliated with or endorsed by
Apple Inc. AirPlay, HomePod, and Apple TV are trademarks of Apple Inc.,
used here only to describe compatibility.*
