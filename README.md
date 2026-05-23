# E7 Shop Refresher

Automation tool for Epic Seven's **Boutique secrète / Secret Shop** on the
STOVE PC client. Detects bookmark / mystic medal / covenant icons, buys
them automatically, refreshes the shop, repeats.

Built in Rust. Single-window egui GUI, no installer, no telemetry.

---

## ⚠️ Important — read this before using

Epic Seven's [Terms of Service](https://page.onstove.com/epicseven/global)
prohibit third-party automation tools. **Using this software risks your
account being banned by Smilegate / STOVE.** The author makes no warranty
and accepts no liability for account losses. Use at your own risk.

Beyond ToS, also be aware that:

- Your screen will be captured continuously while the bot runs (the
  capture stays local — nothing is uploaded).
- Your mouse will move on its own; **don't try to use the PC for
  anything else while the bot runs**, or your inputs will fight each
  other.
- The bot **always brings the game window to the foreground** before
  clicking, so it will steal focus from whatever you're doing.

If any of that is a no-go for you, stop here.

---

## What it does

Per round (configurable up to `max_refreshes`):

1. Wait for the shop screen to be visible (anchor template match).
2. Scroll to top, walk down the item list.
3. For each visible row matching one of the enabled item types
   (mystic medals, covenant bookmarks), click the buy button on that
   row, confirm the modal that pops up.
4. Click **Rafraîchir** (Refresh), confirm the gem cost.
5. Pause (human-ish delays), repeat.

Stops automatically after `max_refreshes`, or when you click Stop, or if
the shop screen disappears.

## How it works

- **Capture** — Windows Graphics Capture (via `xcap`) grabs frames of the
  game window without screen-recording the whole desktop.
- **Detection** — Normalized cross-correlation template matching (via
  `imageproc`), with a 2-stage coarse-to-fine pyramid to keep the cost
  low. The user supplies the templates (cropped icons of the items
  they want to buy).
- **Clicks** — `enigo` synthesises mouse motion (curved, eased, jittered)
  and button events at the OS level. The window is brought to the
  foreground first via `SetForegroundWindow` so clicks land in-game.
- **Buttons at fixed positions** (refresh, modal confirms, buy column)
  are handled via user-drawn click **zones** rather than templates —
  much faster and more robust than NCC for things that don't move.
- **GUI** — `eframe`/`egui` for the control panel, ROI/zone editor,
  template cropping tool, and detection debugger.

## Requirements

- Windows 10/11 (other OSes won't build — `xcap`'s WGC backend is
  Windows-only).
- Epic Seven on the **STOVE PC client** (emulators not supported).
- Rust toolchain (stable, 1.84+ recommended) — see
  https://rustup.rs/ if you don't have it.
- A working display configured at any resolution you like. The bot
  calibrates against your actual window size; it does not require
  1080p.

## Quick start

```powershell
# Clone and build
git clone <repo-url> e7-shop-refresher
cd e7-shop-refresher
cargo build --release

# Launch Epic Seven, open the secret shop screen, then:
.\target\release\gui.exe
```

The GUI will tell you what's missing (templates not cropped, zones not
drawn) and refuse to start until everything is calibrated. Follow the
**First-time setup** below.

## First-time setup

The bot needs three things calibrated to your client:

1. **Templates** — small PNGs of each item's icon. You crop them once
   from a live screenshot.
2. **Zones** — rectangles you draw around the buttons the bot needs to
   click (refresh button, confirm modal buttons, the buy column).
3. **Regions** — search-area rectangles for the templates (shop grid
   for items, top-left header for the anchor).

The included `config.toml` has sensible defaults for **1920×1080**. If
your window matches that, you can skip the regions step. Otherwise,
fine-tune via the GUI.

### Step 1 — Templates

You need three template PNGs in the `templates/` folder:

- `shop_header.png` — the small "Boutique secrète" / "Secret Shop"
  banner at the top-left of the shop screen.
- `mystic_medal.png` — the mystic medal item icon (the small green
  medallion shown in the row when one is on sale).
- `covenant.png` — the covenant bookmark item icon (the small pink/red
  bookmark icon).

To create them:

1. Open the secret shop screen in-game.
2. Click **Refresh** in the GUI's Snapshot panel — a snapshot of the
   game window appears.
3. **Drag** a rectangle on the snapshot tightly around the icon you
   want to capture. Stay close to the icon edges — including too much
   background makes the template match too loosely.
4. In the **Crop & Save** panel, pick the target alias from the
   dropdown (e.g. `mystic_medal`).
5. Click **Save crop**. The PNG is written to `templates/<alias>.png`.

Repeat for all three templates.

> **Tip:** refresh the shop a few times until an actual mystic medal or
> covenant appears — you need a real icon to crop. The `shop_header`
> template never changes, you can crop it any time.

### Step 2 — Zones

Zones tell the bot where to click for buttons whose position doesn't
move:

| Zone | What it covers |
|---|---|
| `refresh` | The "Rafraîchir" / "Refresh" button at the bottom-left. |
| `refresh_confirm` | The blue "Confirmer" / "Confirm" button inside the refresh modal. |
| `buy_confirm` | The large green "Acheter" pill inside the buy modal. |
| `buy_column` | The vertical column where every row's small "1/1 Acheter" button lives. Only the X range matters — the Y is derived from the matched item icon at runtime. |

To draw a zone:

1. Refresh the snapshot if needed.
2. In the **Zones** panel, click **Draw** next to the zone name.
3. Drag a rectangle on the snapshot around the button.
4. The drawn rect is saved to the in-memory config immediately. The
   rectangle is shown as a coloured overlay.

For `refresh_confirm` and `buy_confirm` — those modals aren't visible
in the snapshot of the shop screen. **Draw the zone over the area
where the modal will appear** (centred middle-bottom of the window for
the buy modal; same general area for the refresh modal). The button
will land in that zone when the modal opens at runtime.

For `buy_column` — draw a tall, narrow rectangle covering the column
of green "1/1 Acheter" buttons on the right side of the grid.

When all four zones are set, click **💾 Save zones to config.toml** to
persist.

### Step 3 — Regions (optional)

The defaults are tuned for 1920×1080. If your game window is a
different size and the bot reports missed anchor detection, fine-tune:

- `shop_grid` — the rectangle that contains the item rows. Must
  include all of the icon column, item names, and the buy button
  column.
- `anchor_shop` — a small rectangle around the "Boutique secrète"
  header at the top-left.

Use **Run detection** in the snapshot panel to verify — the bot will
draw boxes around what it matches inside these regions. If the boxes
fall on the right items, you're done.

### Step 4 — Run

Once everything is green (no missing templates, no missing zones),
**▶ Start** lights up. Click it. The bot takes over the mouse and
starts buying / refreshing.

Click **⏹ Stop** at any time. The bot finishes the current click then
exits cleanly within ~1 second.

## Daily use

After the first-time setup, daily use is:

1. Open the shop in-game.
2. Open `gui.exe`.
3. Hit **Start**.
4. Wait until rounds finish or the gem budget is exhausted.

The `config.toml` is preserved across runs.

## Configuration

`config.toml` is documented inline. Key knobs:

| Section | Key | Effect |
|---|---|---|
| `[shop]` | `max_refreshes` | Stop after N rounds. |
| `[shop]` | `buy_mystic_medals` / `buy_covenant` | Toggle each item type. |
| `[shop]` | `buy_button_y_offset_ratio` | Vertical offset from icon to button (E7's button sits below the icon). |
| `[matching]` | `threshold` | NCC acceptance (0.90 by default). Raise if too many false matches; lower if real items are missed. |
| `[timing]` | `click_delay_*` | Inter-click delays — log-normal distribution. |
| `[timing]` | `modal_open_pause_ms` | How long to wait for a modal to render. |
| `[timing]` | `scroll_amount` / `scroll_pause_ms` | Per-scroll lines and settle time. |
| `[timing]` | `long_pause_every_n` | Take a longer break every N rounds. Set 0 to disable. |
| `[window]` | `title_contains` | Window title substring used to find the game. |
| `[window]` | `auto_resize` | Force the window to `base_resolution` at startup. Off by default. |

## Tools

Two helper binaries ship alongside the GUI:

- **`gui.exe`** — the main control panel. What you use day-to-day.
- **`e7-shop-refresher.exe`** — headless CLI runner. Same behaviour as
  the GUI but no window. Useful for scripting. `--dry-run` to validate
  the config without clicking.
- **`grab.exe`** — capture the game window to a PNG with the ROIs
  drawn as overlays. Useful for debugging region calibration without
  the GUI.

```powershell
.\target\release\grab.exe                 # writes captures/snapshot_<ts>.png
.\target\release\e7-shop-refresher.exe    # CLI runner
.\target\release\e7-shop-refresher.exe --dry-run
```

## Troubleshooting

**"Game window not found"** — make sure Epic Seven is running and the
window title matches `[window].title_contains` (default: "Epic Seven").
Check via the Windows window list.

**"Templates missing"** — the GUI's status bar lists which PNGs are
expected. Re-crop them via the Crop & Save panel.

**"Zones not drawn"** — open the Zones panel and Draw each one.
Without all four, Start is disabled.

**Bot clicks just next to a button** — your `buy_column` zone is too
wide or `buy_button_y_offset_ratio` is off. Click **🔍 Run detection**
to see exactly where the click would land — a red translucent band
appears at the computed buy spot. Tune the zone or the ratio until
the band sits on the button.

**Bot keeps trying to buy the same item across scrolls** — usually a
template that's too loose (false matches). Try:
1. Re-crop the template tighter, focusing on the icon center only.
2. Bump `[matching].threshold` to 0.92 or 0.93.
3. Use **Run detection** to see the NCC score on each match — anything
   below 0.95 on a real item suggests the template needs work.

**Modal doesn't open / clicks wrong target** — the modal-verification
hash check should skip these silently (you'll see
`buy modal did not open` in the logs). If you see actual phantom
purchases, the zones are bleeding into wrong UI elements; re-draw
them tighter.

**Bot freezes for several seconds** — likely a `xcap` WGC frame pool
issue. Make sure `auto_resize = false` (the default). If you want
auto-resize, the game window will be SetWindowPos'd at startup
**before** the capture session is created.

**Stop button is unresponsive** — all blocking calls poll the stop
flag every ~60ms, so worst case Stop should take ~200ms. If it takes
much longer, it's stuck in a click animation — the GUI's join is
best-effort, the OS will reap the thread on exit anyway.

## Building from source

```powershell
# Debug build
cargo build

# Release build (recommended — ~5× faster NCC)
cargo build --release

# Run lints (CI-equivalent)
cargo clippy --all-targets -- -D warnings
```

## Architecture

```
src/
├── lib.rs            init(): DPI awareness + rayon thread pool
├── main.rs           CLI entry point
├── bin/grab.rs       Standalone snapshot+overlay tool
├── capture.rs        xcap WGC capture, Win32 foreground / resize
├── detector.rs       NCC pyramidal template matching
├── input.rs          Clicker: human-ish mouse motion, scroll, foreground guard
├── shop.rs           Main loop: anchor wait, scroll, buy, refresh
├── error.rs          Typed errors (thiserror)
├── config.rs         Top-level Config + load/validate
├── config/
│   ├── sections.rs   TOML schema (Window, Shop, Timing, Matching, Regions, Zones, Templates)
│   └── validate.rs   Cross-field checks + missing template / zone lists
└── gui/
    ├── mod.rs        eframe boot + tracing subscriber
    ├── app.rs        ShopGui — controls, snapshot view, region/zone/crop panels
    ├── bot.rs        Worker thread spawn + stop-flag plumbing
    ├── state.rs      Shared stats (round, items bought)
    └── logs.rs       In-memory log buffer + tracing writer
```

Key invariants:

- `Detector::find` is the only template-matching path; `wait_for`
  loops over it.
- Buy buttons are NEVER template-matched at runtime — they are click
  zones with optional pre/post hash checks to verify the modal
  opened.
- A `bought_types` HashSet caps each round to one buy per item type
  (the shop carries at most one of each per refresh).
- An `attempted_icons` HashSet tracks the icon hash of every clicked
  row, so a buy that fails the modal check at view N is skipped at
  views N+1, N+2 (same item that scrolled into another view).
- Both sets reset at round start.
- All long-running loops poll an `Arc<AtomicBool>` stop flag so Stop
  responds within ~200ms.

## License

MIT. See `Cargo.toml` for the SPDX identifier.

This software is provided "as is" without warranty of any kind. The
author is not affiliated with Smilegate, Super Creative, or STOVE.
