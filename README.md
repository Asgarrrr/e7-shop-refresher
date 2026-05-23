# E7 Shop Refresher

Automation tool for Epic Seven's Secret Shop on the STOVE PC client.
Detects mystic medal and covenant bookmark icons, purchases them,
refreshes the shop, repeats.

Written in Rust. Single-window egui GUI, no installer, no telemetry.

---

## Important — read before use

Epic Seven's [Terms of Service](https://page.onstove.com/epicseven/global)
prohibit third-party automation tools. Using this software risks your
account being banned by Smilegate / STOVE. The author makes no warranty
and accepts no liability for account losses. Use at your own risk.

Operational notes:

- The screen is captured continuously while the bot runs. Captures
  stay local — nothing is uploaded.
- The mouse moves on its own. Do not use the PC for anything else
  while the bot is active; inputs will fight each other.
- The game window is brought to the foreground before every click, so
  the bot steals focus from whatever else is active.

---

## What it does

Per round, up to `max_refreshes` or until another stop condition fires:

1. Wait for the shop screen (anchor template match).
2. Scroll to the top of the item list, then walk down.
3. For each visible row matching an enabled item type, click the
   row's buy button and confirm the modal.
4. Click the Refresh button and confirm the gem cost.
5. Pause briefly, then repeat.

The loop stops when any configured stop condition fires
(`max_refreshes`, `stop_after_minutes`, `stop_when_mystic_medals`,
`stop_when_covenants`), when Stop is clicked, or when the shop screen
disappears. If `sleep_when_done` is enabled and a stop condition
fires, the PC is suspended afterwards. Manual Stop never suspends.

## How it works

- **Capture.** Windows Graphics Capture (via `xcap`) grabs frames of
  the game window without screen-recording the desktop.
- **Detection.** Normalised cross-correlation template matching (via
  `imageproc`), with a two-stage coarse-to-fine pyramid. Templates
  are user-supplied crops of each item icon.
- **Clicks.** `enigo` synthesises curved, eased, jittered mouse motion
  and button events at the OS level. The window is brought to the
  foreground first via `SetForegroundWindow`.
- **Fixed-position buttons** (refresh, modal confirms, buy column) are
  driven by user-drawn click zones rather than template matching —
  faster and more robust for UI elements that do not move.
- **GUI.** `eframe` / `egui`, organised into two tabs:
  - **Run** — start / stop, live stats, stop-condition controls, logs.
  - **Setup** — snapshot view, template cropping, zone editor, region
    editor, detection tuning.

## Requirements

- Windows 10 / 11. Other operating systems will not build — `xcap`'s
  WGC backend is Windows-only.
- Epic Seven on the STOVE PC client. Emulators are not supported.
- Rust toolchain (stable, 1.84 or later) — see https://rustup.rs/.
- Any display resolution. The bot calibrates against the live window
  size; 1080p is not required.

## Quick start

```powershell
git clone <repo-url> e7-shop-refresher
cd e7-shop-refresher
cargo build --release

# Launch Epic Seven, open the secret shop, then:
.\target\release\gui.exe
```

On first launch the GUI reports what is missing (templates not
cropped, zones not drawn) and refuses to start until calibration is
complete. See **First-time setup** below.

## First-time setup

Three pieces of calibration are required:

1. **Templates** — small PNG crops of each item icon, captured once
   from a live snapshot via the GUI.
2. **Zones** — rectangles around the buttons the bot needs to click
   (refresh, both confirm modals, the buy column).
3. **Regions** — optional search-area rectangles for the template
   matcher. The shipped defaults target 1920×1080; tune via the GUI
   if your window differs and detection misses.

All three live in the **Setup** tab.

### Step 1 — Templates

No templates are bundled with the project by design — each user must
crop their own so the NCC match stays tight against the live client
(resolution and language). Crop three PNGs into `templates/`:

- `shop_header.png` — the Secret Shop banner at the top-left.
- `mystic_medal.png` — the small green medal icon shown in a row
  when a mystic medal is on sale.
- `covenant.png` — the small pink/red bookmark icon shown in a row
  when a covenant bookmark is on sale.

Procedure:

1. Open the secret shop in-game.
2. In the **Snapshot** panel, click **Refresh** to capture the game
   window.
3. Drag a rectangle on the snapshot, tight around the icon to
   capture. Including background loosens the match.
4. In **Crop & Save**, pick the target alias from the dropdown.
5. Click **Save crop**. The PNG is written to
   `templates/<alias>.png`.

`templates/examples/` contains visual references showing what a good
crop looks like. They are not usable as defaults; copying them as-is
will fail the NCC threshold on any client that does not exactly match
their resolution and language.

> Mystic medals and covenant bookmarks only appear when in stock.
> Refresh in-game until one is present before cropping. The shop
> header is always visible and can be cropped at any time.

### Step 2 — Zones

Zones mark where to click for buttons whose position does not change:

| Zone | What it covers |
|---|---|
| `refresh` | The Refresh button at the bottom-left of the shop screen. |
| `refresh_confirm` | The Confirm button inside the refresh modal. |
| `buy_confirm` | The large Buy button inside the buy modal. |
| `buy_column` | The vertical column of per-row buy buttons. Only the X range is used — Y comes from the matched icon at click time. |

To draw a zone:

1. Refresh the snapshot if needed.
2. In the **Zones** panel, click **Draw** next to the zone name.
3. Drag a rectangle on the snapshot around the button.
4. The rect is auto-saved to `config.toml`. Coloured overlays on the
   snapshot show what is currently set.

`refresh_confirm` and `buy_confirm` are not visible in the shop-screen
snapshot. Draw their zones over the area where the modal appears at
runtime (centred middle-bottom of the window for the buy modal, same
general area for the refresh modal).

For `buy_column`, draw a tall, narrow rectangle covering the column
of per-row buy buttons on the right side of the grid.

### Step 3 — Regions (optional)

If the bot reports missed anchor detection at a non-1080p window
size, tune:

- `shop_grid` — the rectangle containing the item rows. Must include
  the icon column, item names, and the buy button column.
- `anchor_shop` — a small rectangle around the shop header at the
  top-left.

Use **Run detection** in the Snapshot panel to verify. The GUI draws
boxes around what it matched inside these regions.

### Step 4 — Run

When the Run tab shows green (no missing templates, no missing
zones), the Start button becomes enabled. Click it; the bot takes
over the mouse.

Click Stop at any time. The bot finishes the current click and exits
cleanly within roughly one second.

## Daily use

After first-time setup:

1. Open the shop in-game.
2. Open `gui.exe`.
3. Click Start.
4. Wait until rounds finish or a stop condition fires.

`config.toml` is preserved across runs. Slider and toggle changes in
the GUI auto-save with a short debounce.

## Configuration

`config.toml` is documented inline. Key knobs:

| Section | Key | Effect |
|---|---|---|
| `[shop]` | `max_refreshes` | Stop after N rounds. `0` disables. |
| `[shop]` | `stop_after_minutes` | Stop after N elapsed minutes (checked at round boundaries; may overshoot by one round). `0` disables. |
| `[shop]` | `stop_when_mystic_medals` | Stop once N mystic medals have been bought this run. `0` disables. |
| `[shop]` | `stop_when_covenants` | Stop once N covenant bookmarks have been bought this run. `0` disables. |
| `[shop]` | `sleep_when_done` | Suspend the PC after a configured stop condition fires. Never triggers on manual Stop. |
| `[shop]` | `buy_mystic_medals` / `buy_covenant` | Toggle each item type. |
| `[shop]` | `buy_button_y_offset_ratio` | Vertical offset (window-height fraction) from the icon centre to the row's buy button. |
| `[shop]` | `max_scrolls_per_round` | Scroll-downs per round. Six items fit in two views, so one is usually sufficient. |
| `[matching]` | `threshold` | NCC acceptance (default 0.90). Raise to reduce false matches; lower if real items are missed. |
| `[matching]` | `margin` | Required gap between best and runner-up scores for an unambiguous match. |
| `[timing]` | `click_delay_*` | Inter-click delays, log-normal distribution. |
| `[timing]` | `modal_open_pause_ms` | Wait time after a click before hashing the confirm zone. |
| `[timing]` | `scroll_amount` / `scroll_pause_ms` | Per-scroll lines and settle time. |
| `[timing]` | `long_pause_every_n` | Take a longer pause every N rounds. `0` disables. |
| `[window]` | `title_contains` | Window-title substring used to find the game. |
| `[window]` | `process_name` | Optional executable-name filter to disambiguate when multiple windows share the title fragment. |
| `[window]` | `auto_resize` | Force the window to `base_resolution` at startup. Off by default — calibrating at the native size avoids fighting Windows decorations and DPI rounding. |

If at least one stop condition is non-zero, the bot stops when the
first one fires. All four at zero means run until manual Stop.

## Tools

Two binaries are produced by `cargo build --release`:

- `gui.exe` — control panel. Primary entry point for normal use.
- `e7-shop-refresher.exe` — headless CLI runner. Same behaviour as
  the GUI without the window; for scripted or scheduled runs.
  `--dry-run` validates the config without clicking.

```powershell
.\target\release\e7-shop-refresher.exe              # run with default config.toml
.\target\release\e7-shop-refresher.exe --dry-run    # validate config, no clicks
```

## Troubleshooting

**"Game window not found"** — confirm Epic Seven is running and the
window title contains the `[window].title_contains` substring (default
`Epic Seven`).

**"Templates missing"** — the GUI lists which files are expected and
where. Re-crop them via Crop & Save, then click Recheck on the
Templates panel.

**"Zones not drawn"** — open the Zones panel and Draw each one. Start
remains disabled until all four are set.

**Bot clicks just next to a button** — the `buy_column` zone is too
wide, or `buy_button_y_offset_ratio` is off. Run detection from the
Snapshot panel; a translucent red band shows where the click would
land. Tune the zone or the ratio until the band sits on the button.

**Bot keeps trying to buy the same item across scrolls** — usually a
template that is too loose (false matches):

1. Re-crop the template tighter, focused on the icon centre.
2. Raise `[matching].threshold` to 0.92 or 0.93.
3. Run detection shows the NCC score per match. A real item scoring
   below ~0.95 indicates the template needs work.

**Modal does not open / wrong target is clicked** — the
modal-verification hash check should skip these silently; the log
records `buy modal did not open`. Visible phantom purchases mean a
zone is bleeding into the wrong UI element; redraw it tighter.

**Bot freezes for several seconds** — typically an `xcap` WGC
frame-pool issue. Keep `auto_resize` off (the default). If auto-resize
is enabled, `SetWindowPos` is issued at startup, before the capture
session is created.

**Stop appears unresponsive** — blocking calls poll the stop flag
every ~60 ms; worst case Stop takes ~200 ms. Longer delays indicate a
click animation in progress. The GUI join is best-effort; the OS
reaps the thread on exit.

## Building from source

```powershell
# Debug build
cargo build

# Release build (recommended — roughly 5x faster NCC)
cargo build --release

# CI-equivalent lints and tests
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

## Architecture

```
src/
├── lib.rs            init(): DPI awareness + rayon thread pool
├── main.rs           CLI entry point
├── bin/gui.rs        GUI entry point
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
    ├── panels.rs     Run tab + Setup tab panel rendering
    ├── snapshot.rs   Snapshot view, drag-to-crop, overlay drawing
    ├── persist.rs    Coalesced auto-save back to config.toml
    ├── bot.rs        Worker thread spawn + stop-flag plumbing
    ├── state.rs      Shared stats (round, items bought)
    └── logs.rs       In-memory log buffer + tracing writer
```

Key invariants:

- `Detector::find` is the only template-matching path; `wait_for`
  loops over it.
- Buy buttons are never template-matched at runtime. They are click
  zones with pre / post hash checks to verify the modal opened.
- A `bought_types` HashSet caps each round to one buy per item type;
  the shop carries at most one of each per refresh.
- An `attempted_icons` HashSet tracks the icon hash of every clicked
  row, so a buy that fails the modal check at view N is skipped at
  views N+1 and N+2.
- Both sets reset at round start.
- All long-running loops poll an `Arc<AtomicBool>` stop flag, so Stop
  responds within ~200 ms.

## License

MIT — see [LICENSE](LICENSE).

This software is provided "as is" without warranty of any kind. The
author is not affiliated with Smilegate, Super Creative, or STOVE.
