# Architecture

Code map and invariants for contributors. The user-facing manual lives
in [README.md](README.md).

## Layout

```
src/
├── lib.rs                       init(): DPI awareness + rayon thread pool
├── main.rs                      GUI entry point (e7-shop-refresher.exe)
├── bin/e7-shop-refresher-cli.rs CLI entry point (e7-shop-refresher-cli.exe)
├── capture.rs        xcap WGC capture, Win32 foreground / resize
├── detector.rs       NCC pyramidal template matching
├── input.rs          Clicker: human-ish mouse motion, scroll, foreground guard
├── shop.rs           Main loop: anchor wait, scroll, buy, refresh
├── power.rs          Suspend-to-sleep on completion
├── error.rs          Typed errors (thiserror)
├── config.rs         Top-level Config + load / validate / migrate
├── config/
│   ├── sections.rs   TOML schema (Window, Shop, Timing, Matching, Regions, Zones, Templates)
│   └── validate.rs   Cross-field checks + missing template / zone lists
└── gui/
    ├── mod.rs        eframe boot + tracing subscriber
    ├── app.rs        ShopGui — Run / Setup tabs, shared state
    ├── panels.rs     Run + Setup tab rendering
    ├── snapshot.rs   Snapshot view, drag-to-crop, overlay drawing
    ├── persist.rs    Coalesced auto-save back to config.toml
    ├── bot.rs        Worker thread spawn + stop-flag plumbing
    ├── state.rs      Shared stats (round, items bought, sub-status)
    └── logs.rs       In-memory log buffer + tracing layer
```

## Invariants

- `Detector::find` is the only template-matching path; `wait_for`
  loops over it with a stop-flag check between polls.
- Buy buttons are never template-matched at runtime. They are click
  zones with pre / post hash checks to verify the modal opened.
- `bought_types` HashSet caps each round to one buy per item type
  (the shop carries at most one of each per refresh).
- `attempted_icons` HashSet tracks the icon hash of every clicked
  row, so a buy that fails the modal check at view N is skipped at
  views N+1 and N+2.
- Both sets reset at round start.
- Long-running loops poll an `Arc<AtomicBool>` stop flag; Stop
  responds within ~200 ms.
- Run-tab parameters (stop conditions, item targets, `sleep_when_done`)
  ride through a shared `Arc<RwLock<ShopConfig>>` re-read at every
  round boundary, so edits land live without a restart.
- `sleep_when_done` is one-shot: the worker signals `sleep_consumed`
  after `power::suspend_to_sleep()` returns, the GUI clears the
  checkbox. The next run never sleeps silently.
