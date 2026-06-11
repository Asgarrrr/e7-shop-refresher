# Setup walkthrough

The bot runs out of the box on a stock STOVE client — click positions,
the item-grid search region, and the two item icon templates all ship
inside the binary. This doc covers the cases where the bundled defaults
miss on your client, plus the manual overrides available in
**Setup → Advanced overrides**.

If detection works on your first Start, you can skip this whole page.

---

## Before you start

- Epic Seven is running on the STOVE PC client.
- You've accepted the UAC prompt when launching `e7-shop-refresher.exe`
  (admin rights — needed because the game runs elevated and Windows
  drops clicks from a lower-privilege process).
- You're on the Secret Shop screen in game.

The Setup tab's **Layout** card carries a "Bundled layout — no setup
required" status when the defaults are in effect. The **Advanced
overrides** card below lets you replace individual elements when
something misses.

---

## Snapshot — the canvas for everything else

In the **Snapshot** card, click **Refresh**. The app captures one
frame from the Epic Seven window. That frame is the canvas you'll drag
on for every override below.

If the snapshot is blank or shows the wrong window, check that Epic
Seven is in the foreground when you click — or that
`[window].title_contains` matches your client's window title (default
`Epic Seven`, edit in `config.toml` if your client uses a different
one).

---

## Verifying detection

Click **Run detection** in the Snapshot card. It re-snapshots and runs
the NCC matcher in one shot. A **Last detection** block appears under
the buttons:

```
Last detection
  mystic_medal: score=0.972 margin=0.089 @ (812, 245)
  covenant: no match
```

Real items should score ≥ 0.95. Below that, the template needs
re-cropping. `no match` on a row that should have one means either the
template is wrong or `[matching].threshold` is too high.

On the snapshot itself, Run detection draws a bounding box around each
match with the alias and score above it.

**Button Y offset check.** Hover the **Button Y offset** row in the
Snapshot card after Run detection — a red band overlays the snapshot
where buy clicks would land. It should sit cleanly on the buy buttons.
Tune the value until the band aligns.

---

## Advanced overrides

Click positions and the item-grid search region are bundled defaults
from `crate::layout`. Override them in **Setup → Advanced overrides**
when the bundled values miss on your setup.

### Reference templates

| Alias | What it is |
|---|---|
| `mystic_medal` | The green medal icon. Only visible when one is in stock. |
| `covenant` | The pink / red bookmark icon. Only visible when one is in stock. |

The bundled fallbacks were cropped on the French STOVE client at
1433×837 and survive most resolutions thanks to runtime resampling. If
yours misses, re-crop:

1. **Snapshot** with a mystic medal or covenant visible in the shop.
2. **Advanced overrides → Reference templates → Edit** next to the
   alias. The button toggles to *Cancel*.
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

### Search regions

`shop_grid` is the only search region the user can override — it tells
the Detector where the item column is. The bundled default works on
1080p / 1906×1103. Set it tighter if a same-coloured icon outside the
item column is false-matching.

---

## After a game update

Smilegate occasionally retouches UI assets. If matches start failing
after a patch:

1. Take a fresh snapshot.
2. Identify the offending element with **Run detection**.
3. Re-crop the affected template or redraw the affected zone.

Click positions don't change unless you change resolution.
