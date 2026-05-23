# Templates

Required PNGs the bot template-matches against frames captured from the
Epic Seven window. **You crop these once from a live screenshot** — they
must look exactly like what your client renders.

## Required files

The bot won't start until these three are present in this folder:

| File | What it is |
|---|---|
| `shop_header.png` | The "Boutique secrète" / "Secret Shop" banner at the top-left of the shop screen. |
| `mystic_medal.png` | The small green medal icon that appears in a row when a mystic medal is on sale. |
| `covenant.png` | The pink/red bookmark icon for covenant bookmarks. |

## Cropping rules of thumb

- **Tight crops** — include the icon and only the icon. Adding
  background, drop shadows, or adjacent text loosens the match and
  causes false positives.
- **Use the GUI's Crop & Save tool**, not Photoshop. The GUI captures
  the same WGC frames the bot will see at runtime, so colour
  reproduction and resolution match exactly.
- **One refresh = one chance** — if the shop doesn't currently have a
  mystic medal or covenant on sale, refresh in-game until one shows up,
  then crop. The `shop_header` is always visible so it can be cropped
  any time.
- **Recheck after game updates** — Smilegate occasionally tweaks the
  UI. If matches start failing after a patch, recrop.

## `examples/`

Reference crops made at **1906×1103** on the **French STOVE client**.
You can drop them in this folder as-is if your client matches that
configuration exactly. They're checked in as visual references, not as
working defaults — copy and rename to use them.

```powershell
Copy-Item examples\covenant_1906x1103_fr.png covenant.png
Copy-Item examples\mystic_medal_1906x1103_fr.png mystic_medal.png
Copy-Item examples\shop_header_1906x1103_fr.png shop_header.png
```

For any other resolution or language, **recrop your own** via the GUI —
don't rely on these examples, the match threshold will reject them.

## When the bot says "templates missing"

The GUI's status bar lists exactly which files are expected and where.
The Templates panel also has a **Recheck** button that re-scans this
folder without restarting.
