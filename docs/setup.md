# Setup walkthrough

The bot runs out of the box on a stock STOVE client — click positions,
the item-grid search region, and the two item icon templates all ship
inside the binary. This doc covers the cases where the bundled defaults
miss on your client, plus the manual overrides available in the Setup
tab's **Layout** card.

If detection works on your first Start, you can skip this whole page.

![The Setup tab with the layout overlay on, showing the search region,
click zones, and reference templates beside the live snapshot](screenshots/setup-tab.png)

---

## Before you start

- Epic Seven is running on the STOVE PC client.
- You've accepted the UAC prompt when launching `e7-shop-refresher.exe`
  (admin rights — needed because the game runs elevated and Windows
  drops clicks from a lower-privilege process).
- You're on the Secret Shop screen in game.

The Setup tab is split into three cards: **Detection** (live snapshot
size, NCC threshold, and the running match list), **Layout** (the search
region, click zones, and reference templates you can override), and
**Timing** (pacing knobs, including **Yield to user (idle ms)** —
cooperative mode pauses the bot when you touch the mouse/keyboard and
resumes after that idle window; `0` disables it). Every override below
lives in the **Layout** card.

With **Show layout overlay on snapshot** ticked, every region the bot
looks in and every point it clicks is drawn over the central snapshot —
the quickest way to spot what's drifted before you touch a single value.

---

## The snapshot — the canvas for everything else

The central snapshot refreshes on its own while the Setup tab is open —
the **Detection** card shows "live · every N ms" (tunable). That frame
is the canvas you'll drag on for every override below; there's no button
to press.

If the snapshot is blank or shows the wrong window, check that Epic
Seven is running and that `[window].title_contains` matches your
client's window title (default `Epic Seven`, edit in `config.toml` if
your client uses a different one).

---

## Verifying detection

Detection runs continuously. The **Detection** card lists every item
found in the current frame with its score and margin, and the snapshot
draws a box around each match:

![A covenant bookmark detected on the snapshot, with "covenant score
0.979 · margin inf" listed in the Detection card](screenshots/detection-hit.png)

Real items should score ≥ 0.95. Below that, the template needs
re-cropping. `margin` reads `inf` when there's no runner-up — a single
confident match — and shrinks toward zero when two regions correlate
about equally (an ambiguous match the bot rejects). A blank list while
an item is on screen means either the template is wrong or the **NCC
threshold** is too high. The same hue-histogram check the bot uses runs
here too, so an NCC hit on a wrong-coloured icon is dropped from the
list rather than shown. If a *correct* item is being dropped — the log
shows `rejected by colour check` with `likely_screen_tint=true` — a
global screen colour cast (Windows Night Light, an ICC/HDR profile) is
shifting the hues; disable it or raise `matching.colour_match_threshold`
toward 0.8.

**Buy-click alignment.** The red **Buy reference** line and **Buy click**
box are always drawn on the snapshot. Drag the line onto the centre of
any item row, then drag the box down onto that row's Buy button — every
buy click lands at a random point inside the box. Rough alignment of the
line is enough; the bot finds each item's real Y live.

---

## Layout overrides

Click positions and the item-grid search region are bundled defaults
from `crate::layout`. Override them in the **Setup → Layout** card when
the bundled values miss on your setup. Tick **Show layout overlay on
snapshot** to draw every search region (green) and click point (orange)
over the snapshot while you tune.

### Reference templates

| Alias | What it is |
|---|---|
| `mystic_medal` | The green medal icon. Only visible when one is in stock. |
| `covenant` | The pink / red bookmark icon. Only visible when one is in stock. |

The bundled fallbacks were cropped on the French STOVE client at
1433×837 and survive most resolutions thanks to runtime resampling. If
yours misses, re-crop:

1. Get a mystic medal or covenant **visible in the shop** so it shows on
   the live snapshot.
2. **Layout → Reference templates → Edit** next to the alias. The button
   toggles to *Cancel*.
3. **Drag** a tight rectangle on the snapshot. The Detector reloads on
   release.

The medal and covenant icons only appear when one is in stock — refresh
in-game until at least one shows up before snapshotting.

**Cropping tips:**

- **Tight.** Include the icon and only the icon. Drop shadows or
  adjacent text loosen the match.
- **Use the GUI, not Photoshop.** Photoshop crops differ from the
  capture frame (colour profile, scaling) and silently fail the match.

The drag-pick save records the current game window resolution into
`[window].base_resolution`. If you crop one template, resize the
Epic Seven window, then crop another, the Detector resamples them at
the wrong ratio. Keep all overrides at the same window size.

### Click zones

| Target | Where |
|---|---|
| `refresh` | The *Refresh* button, bottom-left of the shop. |
| `refresh_confirm` | The *Confirm* button inside the refresh modal. |
| `buy_confirm` | The *Buy* button inside the buy modal. |
| `buy_column` | A tall narrow rectangle covering the column of per-row buy buttons. Only the X range is used — Y comes from the matched icon. |

Same drag-on-snapshot flow as templates. `refresh_confirm` and
`buy_confirm` aren't visible on the shop screen — draw them over the
area *where the modal will appear* at runtime (centre of the screen,
on the modal buttons).

**Keep each zone inside its real button.** Smaller than the button is
fine — every click picks a random point inside the zone, so a snug box
just tightens the spread. Spilling past the button's edge is not: a
click can then land on dead space or the wrong control and the round
fails. Turn the overlay on and check that no orange click zone pokes
outside its button face, and that the green search regions sit squarely
over the right area before you start a run.

### Search regions

`shop_grid` is the only search region the user can override — it tells
the Detector where the item column is. The bundled default works on
1080p / 1906×1103. Set it tighter if a same-coloured icon outside the
item column is false-matching.

---

## After a game update

Smilegate occasionally retouches UI assets. If matches start failing
after a patch:

1. Open the Setup tab and watch the live snapshot.
2. Identify the offending element in the **Detection** match list.
3. Re-crop the affected template or redraw the affected zone.

Click positions don't change unless you change resolution.
