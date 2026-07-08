# Architecture

Code map and invariants for contributors. The user-facing manual lives
in [README.md](README.md), and the release history in
[CHANGELOG.md](CHANGELOG.md).

## Layout

```
src/
├── lib.rs                       init(): DPI awareness + rayon thread pool
├── main.rs                      GUI entry point (e7-shop-refresher.exe)
├── bin/e7-shop-refresher-cli.rs CLI entry point (e7-shop-refresher-cli.exe)
│
├── capture.rs       xcap WGC capture, Win32 foreground / resize
├── detector.rs      NCC pyramidal template matching
├── color_check.rs   Hue-histogram FP filter on top of NCC hits
├── input.rs         Clicker: human-ish mouse motion, scroll, foreground guard
├── layout.rs        Bundled click positions + search ROIs (window-relative ratios)
├── shop/
│   ├── mod.rs       ShopRunner lifecycle: run loop, failure cap, webhooks
│   ├── round.rs     Per-round actions: buy, scroll, refresh
│   ├── scan.rs      Row inventory (scan_shop_rows) + strip_hash helpers
│   ├── wait.rs      Screen-state waits: modal dimming, frame settling
│   └── stop.rs      Stop conditions + shop price/drop-rate constants
├── power.rs         Suspend-to-sleep on completion
├── error.rs         Typed errors (thiserror)
├── http.rs          Minimal WinHTTP wrapper (Discord webhook, GitHub API)
├── notifications.rs Discord webhook summaries + send-test plumbing
├── update_check.rs  Background GitHub release check (banner trigger)
├── auto_update.rs   In-place self-update: download, SHA256 verify, swap, restart
│
├── config.rs        Top-level Config + load / validate / migrate
├── config/
│   ├── sections.rs  TOML schema (Window, Shop, Timing, Matching, Regions, Zones, Templates)
│   └── validate.rs  Cross-field checks
│
└── gui/
    ├── mod.rs       eframe boot + tracing subscriber
    ├── app.rs       ShopGui — Run / Setup tabs, shared state, auto-update state
    ├── snapshot.rs  Snapshot view, drag-to-crop, overlay drawing
    ├── persist.rs   Coalesced auto-save back to config.toml (toml_edit)
    ├── bot.rs       Worker thread spawn + stop-flag plumbing
    ├── state.rs     Shared stats (round, items bought, sub-status)
    ├── logs.rs      In-memory log buffer + tracing layer
    ├── hotkey.rs    Ctrl+7 global hotkey via Win32 RegisterHotKey
    ├── panels.rs    Common widgets + tab bar + window footer
    └── panels/
        ├── banner.rs       Update banner + open-url COM trick
        ├── logs_panel.rs   Log buffer rendering + level filter
        ├── parsers.rs      format_/parse_ for minutes / gold / ms
        ├── run_tab.rs      Run tab + action row + stop conditions + stats
        ├── setup_tab.rs    Setup tab + Advanced overrides + snapshot section
        └── timing.rs       Timing knobs (click / motion / pacing / modal)
```

## Invariants

- **Detection is row-anchored.** `Detector::find_all_in` locates every
  buy-button anchor (the locale-independent "1/1" pill segment) in the
  buy column; each row's icon cell — at a fixed offset from its anchor —
  is then classified by an NCC `find` restricted to that cell AND a
  `ColorVerifier` hue check on the matched patch. Both must agree or
  the row is not a target. The click target is the matched anchor
  itself.
- **Waits are observed, never timed.** A modal is open when the shop
  grid's mean luminance drops below 0.85× its pre-click baseline
  (ratio-based, so screen tints cancel); an animation is over when two
  consecutive zone hashes match (`shop/wait.rs`). Poll cadence and
  timeouts are universal constants, not config. On a missed confirm the
  runner clicks the modal's Cancel and counts the action as failed.
- **The buy modal's item icon is reclassified before confirm** — a
  drifted row click cancels out instead of buying the wrong item.
- **`bought_types` HashSet caps each round to one buy per item type** —
  the shop carries at most one of each per refresh. Cleared at round
  start.
- **`refresh_shop` returns `true` only when the items grid actually
  rerolled.** Failed rounds count toward `consecutive_failures` so the
  bot bails rather than hammering a non-shop screen. There is no
  pre-flight "are we in the shop?" check — the modal-dimming checks
  plus the post-refresh `shop_grid` hash cover the IAP-redirect /
  wrong-screen cases.
- **Long-running loops poll an `Arc<AtomicBool>` stop flag.** Stop
  responds within ~200 ms.
- **Run-tab parameters** (stop conditions, item targets,
  `sleep_when_done`) ride through a shared `Arc<RwLock<ShopConfig>>`
  re-read at every round boundary, so edits land live without a restart.
- **`sleep_when_done` is one-shot.** The worker signals
  `sleep_consumed` after `power::suspend_to_sleep()` returns, the GUI
  clears the checkbox. The next run never sleeps silently.
- **Bundled fallbacks scale against `BUNDLED_TEMPLATE_NATIVE_HEIGHT`**
  (the window height the `assets/*.png` were cropped at), not
  `config.window.base_resolution` (which tracks the user's own crops).
- **Auto-update stages `<exe>.new` next to the running binary** so the
  final atomic rename is on the same volume. `cleanup_previous_bak`
  removes leftover `.bak` / `.new` artefacts at startup.
