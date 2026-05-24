# E7 Shop Refresher

Automation for Epic Seven's Secret Shop on the STOVE PC client.
Detects mystic medals and covenant bookmarks, buys them, refreshes
the shop, repeats.

Rust · egui GUI · no installer · no telemetry · Windows only.

---

## Important — read before use

Epic Seven's [Terms of Service](https://page.onstove.com/epicseven/global)
prohibit third-party automation. Using this software risks an account
ban from Smilegate / STOVE. The author makes no warranty and accepts
no liability for losses. Use at your own risk.

- Captures stay local — nothing is uploaded.
- The mouse moves on its own. Don't use the PC while a run is active.
- The game window is brought to the foreground before every click.

## Requirements

- Windows 10 / 11 (the WGC capture backend is Windows-only).
- Epic Seven on STOVE PC. Emulators are not supported.
- Rust toolchain (stable, 1.84+) — https://rustup.rs/
- **Administrator privileges.** STOVE runs the game at an elevated
  integrity level, and Windows UIPI silently drops synthetic input
  from a lower-integrity process. The .exe ships with a manifest
  that triggers UAC on launch; accept the prompt or the bot's
  clicks won't reach the game.

## Quick start

```powershell
git clone <repo-url> e7-shop-refresher
cd e7-shop-refresher
cargo build --release
.\target\release\e7-shop-refresher.exe
```

Config and templates default to `%APPDATA%\e7-shop-refresher\`. Drop
a `config.toml` next to the .exe for portable mode. Override either
with `-c <path>`.

On first launch the GUI lists what's missing and refuses to start
until calibration is complete.

## First-time setup

Calibration is a one-time, ~5 minute job in the **Setup** tab.

### 1. Templates

Crop three PNGs from a live snapshot:

- `shop_header.png` — the Secret Shop banner.
- `mystic_medal.png` — the green medal icon. Only visible when one
  is in stock; refresh in-game until it appears before cropping.
- `covenant.png` — the pink / red bookmark icon. Same caveat.

In the **Templates** card: drag a tight rectangle on the snapshot,
pick the alias, click **Save crop**. The card title flips to
`Templates · ready` once all three are saved. `templates/examples/`
shows reference crops — visual guides, not usable as defaults
(resolution / language mismatch will fail NCC).

### 2. Click targets

Four buttons whose position doesn't change. In the **Click targets**
card, click **Draw** next to each name, then drag a rectangle:

| Target | Where |
|---|---|
| `refresh` | Refresh button, bottom-left of the shop. |
| `refresh_confirm` | Confirm button inside the refresh modal. |
| `buy_confirm` | Buy button inside the buy modal. |
| `buy_column` | The vertical column of per-row buy buttons. Only the X range is used — Y comes from the matched icon. |

`refresh_confirm` and `buy_confirm` aren't visible on the shop
screen; draw them over the area where the modal appears at runtime.
For `buy_column`, draw a tall narrow rectangle covering the column
of per-row buy buttons.

### 3. Search regions (optional)

Tune `shop_grid` and `anchor_shop` if your window isn't 1080p and
detection misses. The **Search regions** card has the same Draw
workflow. Unset = full image, which is fine on 1080p.

### 4. Run

Switch to the **Run** tab. Start unlocks once Window · Snapshot ·
Templates · Click targets are all green. Stop is one click and exits
within ~200 ms.

## Daily use

1. Open the shop in-game.
2. Launch `e7-shop-refresher.exe`.
3. Click **Start**.

Stop conditions and item toggles are live-editable mid-run — the
worker re-reads at every round boundary. `sleep_when_done` suspends
the PC after a stop condition fires (one-shot; manual Stop never
suspends).

## Configuration

Every knob the bot exposes is editable in the GUI. `config.toml` is
the backing store, written automatically (250 ms debounce). Only
`[window]` and `[templates]` need hand-editing.

| Where | Covers |
|---|---|
| **Run** tab | Stop conditions, item targets, `sleep_when_done`. Live-editable mid-run. |
| **Setup → Snapshot** | Capture, **Run detection**, inline NCC threshold + buy-button Y offset (hover the row to preview the click band over last-detected items). |
| **Setup → Templates** | Crop per item icon. |
| **Setup → Search regions** | `[regions]` rectangles. |
| **Setup → Click targets** | `[zones]` rectangles. |
| **Setup → Timing** | `[timing]` — click delays, mouse path, round pacing, jitter, modal & scroll. |
| `config.toml` only | `[window]` (`title_contains`, `process_name`, `auto_resize`), `[templates]` (filenames + directory). |

Stop conditions: any of `max_refreshes`, `stop_after_minutes`,
`stop_when_mystic_medals`, `stop_when_covenants`. Zero disables.
All zero = run until manual Stop.

## Troubleshooting

**Game window not found** — Epic Seven not running, or its title
doesn't contain `[window].title_contains` (default `Epic Seven`).

**Templates missing** — the Templates card title lists how many.
Re-snapshot, drag, pick alias, **Save crop**. No Recheck step.

**Click targets not drawn** — open the panel and **Draw** each.
Start stays disabled until all four are set.

**Bot clicks next to a button** — `buy_column` too wide, or
`buy_button_y_offset_ratio` off. Run detection, then hover **Button
Y offset** in the Snapshot card — the red band shows where clicks
would land. Tune until the band sits on the button.

**False matches across scrolls** — re-crop tighter on the icon
centre; raise `[matching].threshold` to 0.92–0.93. Run detection
shows the NCC score per match; a real item below ~0.95 means the
template needs work.

**Bot freezes for several seconds** — keep `auto_resize` off (the
default). Forcing a resize at startup can hose the WGC frame pool.

**Stop unresponsive** — worst case ~200 ms; longer means a click
animation is in flight. The GUI join is best-effort.

## How it works

- **Capture** — Windows Graphics Capture via `xcap`. No desktop
  recording, no overlay.
- **Detection** — NCC pyramidal template matching via `imageproc`.
  User-supplied crops, two-stage coarse-to-fine.
- **Clicks** — `enigo` synthesises curved, eased, jittered motion at
  the OS level. The window is brought to the foreground first via
  `SetForegroundWindow`.
- **Fixed positions** (refresh, modal confirms, buy column) use
  user-drawn zones rather than template matching — faster and more
  robust for UI chrome.

`e7-shop-refresher-cli.exe` is the headless CLI sibling.
`--dry-run` validates config without clicking.

## Building from source

```powershell
cargo build --release

# CI-equivalent
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

See [ARCHITECTURE.md](ARCHITECTURE.md) for the code map and invariants.

## License

MIT — see [LICENSE](LICENSE).

Not affiliated with Smilegate, Super Creative, or STOVE.
