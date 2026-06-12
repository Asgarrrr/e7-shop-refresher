# Templates

PNGs the bot template-matches against frames captured from the Epic
Seven window. **Both templates ship with the project as bundled
fallbacks**, so a fresh install runs out of the box on the stock STOVE
client. Drop files here only if the bundled fallbacks miss on your setup.

At runtime the bot looks for these files in the templates directory
that sits next to the active config (`%APPDATA%\e7-shop-refresher\templates\`
by default — see the top-level README for portable-mode overrides).
Missing files are not an error: the binary-embedded fallback is used
instead. This in-repo `templates/` directory is only used during
development and as a holder for the `examples/` reference crops.

## Recognized files

| File | What it is |
|---|---|
| `mystic_medal.png` | The small green medal icon shown in a row when a mystic medal is on sale. |
| `covenant.png` | The pink/red bookmark icon shown in a row when a covenant bookmark is on sale. |

## Cropping rules of thumb

If you do need to recrop:

- **Tight crops.** Include the icon and only the icon. Drop shadows,
  background, or adjacent text loosen the match and cause false
  positives.
- **Use the GUI, not Photoshop.** Photoshop crops from a screenshot
  often differ from the WGC frame (colour profile, scaling) and will
  silently fail the NCC threshold.
- **One refresh = one chance.** If the shop has no mystic medal or
  covenant on sale right now, refresh in-game until one appears, then
  crop.
- **Recheck after game updates.** Smilegate occasionally tweaks UI
  assets. If matches start failing after a patch, recrop the affected
  template via **Advanced overrides** in the Setup tab.

## `examples/`

Reference crops taken at **1906×1103** on the **French STOVE client**,
checked in purely as a visual guide of *what a good crop looks like*
(tight bounds, no background, no shadow).

**Do not copy them into this folder as defaults.** They only match a
client that renders at the same resolution and language. Crop your own
via the GUI if the bundled fallbacks miss.
