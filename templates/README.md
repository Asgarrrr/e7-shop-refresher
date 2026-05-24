# Templates

PNGs the bot template-matches against frames captured from the Epic
Seven window. **No templates ship with the project on purpose** — each
user must crop their own from their live client so the match accuracy
stays high.

At runtime the bot looks for these files in the templates directory
that sits next to the active config (`%APPDATA%\e7-shop-refresher\templates\`
by default — see the top-level README for portable-mode overrides).
This in-repo `templates/` directory is only used during development
and as a holder for the `examples/` reference crops.

## Required files

The bot won't start until these three files exist in the active
templates directory:

| File | What it is |
|---|---|
| `shop_header.png` | The Secret Shop banner at the top-left of the shop screen. |
| `mystic_medal.png` | The small green medal icon shown in a row when a mystic medal is on sale. |
| `covenant.png` | The pink/red bookmark icon shown in a row when a covenant bookmark is on sale. |

Make them via the GUI's **Templates** section (Setup tab) — it
captures the same WGC frames the bot sees at runtime, so colour
reproduction and resolution are guaranteed to match.

## Cropping rules of thumb

- **Tight crops.** Include the icon and only the icon. Drop shadows,
  background, or adjacent text loosen the match and cause false
  positives.
- **Use the GUI, not Photoshop.** Photoshop crops from a screenshot
  often differ from the WGC frame (colour profile, scaling) and will
  silently fail the NCC threshold.
- **One refresh = one chance.** If the shop has no mystic medal /
  covenant on sale right now, refresh in-game until one appears, then
  crop. The shop header is always visible — crop it any time.
- **Recheck after game updates.** Smilegate occasionally tweaks UI
  assets. If matches start failing after a patch, recrop.

## `examples/`

Reference crops taken at **1906×1103** on the **French STOVE client**,
checked in purely as a visual guide of *what a good crop looks like*
(tight bounds, no background, no shadow).

**Do not copy them into this folder as defaults.** They will only match
a client that happens to render at the same resolution and language,
and the NCC threshold will reject them otherwise. Crop your own via the
GUI.

## When the bot says "templates missing"

The Templates section title shows the count of missing aliases and the
dropdown labels each row `missing` or `saved`. Saving a new crop or
restarting the app picks up any file added by hand.
