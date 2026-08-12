# Contributing to Playfruit

Thanks for the interest! This is a small, best-effort project — here's how to
help effectively.

## Ground rules

- **Bug reports**: include your Windows version, HomePod model + firmware
  (Home app → speaker settings), what you heard vs expected, and the log file
  (`%APPDATA%\playfruit\playfruit-tray.log`, or console output for the CLI).
- **PRs**: keep them focused. `cargo build --release -p playfruit-cli
  -p playfruit-tray` must pass; CI builds Windows + checks macOS.
- **Vendored code** (`vendor/airplay2-rs`): upstream's code, GPL-2.0. Local
  patches must add a `MODIFIED from upstream` notice at the top of the file
  and an entry in the README's provenance list. Protocol fixes are usually
  better sent upstream first.
- **License**: contributions are accepted under GPL-2.0-or-later.
- **Comments**: inherited code has Spanish comments; new code should use
  English. Translation PRs welcome.

## Where help is most valuable

- Field reports from different HomePod models/firmware and Windows versions
- The Windows capture path (WASAPI edge cases: device switching, exclusive
  mode apps, sample-rate oddities)
- Latency measurements with `click_cli` on different networks
- Keeping up with Apple firmware changes (see how shairport-sync/owntone/pyatv
  track these)
