# Changelog

All notable changes to this project. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.0] — 2026-06-13

### Added

- **In-place auto-updater.** The update banner now offers a *Download &
  restart* button — downloads the new binary, verifies it against the
  release's `SHA256SUMS.txt`, swaps the current `.exe` in place, and
  relaunches. Config and templates are preserved across updates.
- `SHA256SUMS.txt` published with every release for integrity verification.
- Post-refresh items-grid hash check — a round that doesn't actually
  reroll the shop is now counted as failed, so the
  `consecutive_failures` cap eventually trips instead of hammering a
  non-shop screen.
- **Cooperative mode.** The bot yields the cursor the moment you touch
  the mouse or keyboard and resumes only after an idle window
  (*Yield to user (idle ms)*, default 1.5 s; `0` disables). Tune it
  under Setup → Timing.

### Changed

- **Shop screen is no longer detected via an anchor template.** The
  multi-anchor / pre-flight detection proved too brittle across
  languages and resolutions. Safety against IAP-redirect / wrong-screen
  scenarios now comes from the downstream modal-open hash checks plus
  the new post-refresh items-grid check.
- NCC pipeline: shared `SearchContext` across templates in one frame,
  parallel evaluation via rayon, and a confident-miss early-out skip
  the remaining scales when the closest-to-1.0 scale falls far below
  threshold. Big speedup on rounds where neither item is in stock.
- Templates `back_arrow.png`, `refresh_pill.png`, `buy_pill.png` removed
  from the bundle. Only `mystic_medal.png` and `covenant.png` remain.
- `gui/panels.rs` split into six focused submodules (`banner`,
  `logs_panel`, `parsers`, `run_tab`, `setup_tab`, `timing`).
- GUI Layout card replaces the old Detection card — quieter copy,
  matches what's actually shipped (bundled layout + 2 item templates).

### Removed

- `timing.anchor_timeout_ms` and `timing.poll_interval_ms` config
  fields. Stale on-disk values are silently ignored by serde.
- `Detector::wait_for` / `wait_for_shop` / `is_in_shop` /
  `shop_signals_count` and the `ShopSignalRois` struct.
- `Error::AnchorTimeout` variant.
- `regions.back_arrow` config field.

### Fixed

- Cross-volume rename failure during auto-update install (staging on
  C:\ while the app is installed on D:\ now works — the `.new` is
  written next to the running `.exe`).
- Download progress events properly throttled to one per 256 KB. The
  previous logic sent one event per chunk after the first 256 KB.

## [0.6.2] — 2026-XX-XX

### Changed

- Force dark theme so light-mode hosts stay readable.

## [0.6.1] — 2026-XX-XX

### Changed

- Embedded administrator manifest in the binaries — Windows now prompts
  for elevation at launch rather than silently dropping clicks.

## [0.6.0] — 2026-XX-XX

### Added

- `stop_when_gold_spent` stop condition.
- Global `Ctrl+7` emergency-stop hotkey.

### Changed

- Window is forced back to the crop-time resolution at *Start*, so a
  mid-session resize between calibration and run no longer mis-scales
  templates.
- Foreground check now accepts any HWND from the same process — covers
  STOVE's multi-window launcher.
- `SetForegroundWindow` is polled for up to 200 ms — covers the async
  focus-change race.
- Round counter tracks successful refreshes rather than loop iterations.

### Removed

- Run-tab debug demo-stats injector.

[Unreleased]: https://github.com/Asgarrrr/e7-shop-refresher/compare/v0.7.0...HEAD
[0.7.0]: https://github.com/Asgarrrr/e7-shop-refresher/compare/v0.6.2...v0.7.0
[0.6.2]: https://github.com/Asgarrrr/e7-shop-refresher/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/Asgarrrr/e7-shop-refresher/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/Asgarrrr/e7-shop-refresher/releases/tag/v0.6.0
