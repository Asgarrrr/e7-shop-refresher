# Arkyve — Refresh Shop

Local relay for Epic Seven's Secret Shop. The capture side is read-only: it
observes a copy of the game's traffic and forwards the raw stream to an
analysis server; the game's network traffic is never altered. On Windows the
tool drives the shop itself from the decoded snapshots — it refreshes and
buys matched items via click emulation.

## How it works

```
WinDivert SNIFF ─▶ parse IP/TCP ─▶ TCP reassembly ─▶ gate ─▶ WebSocket ─▶ server
   (blocking)                       (ordered/dedup)                  ▲         │
                                                              snapshots ◀──────┘
```

- **Capture**: WinDivert in `SNIFF` + `RECV_ONLY` mode yields a *copy* of the
  game-port TCP packets; the originals continue on their way intact.
- **Reassembly**: captured segments (possibly out of order or retransmitted) are
  recomposed into an ordered byte stream, per connection.
- **Forwarding**: the raw server → client stream is sent as-is to the analysis
  server. Decryption and interpretation happen **server-side** — the client
  decrypts nothing.
- **Control**: each decoded shop snapshot is fed to the refresh-loop
  controller, which checks it against the `[filter]` criteria from
  `config.toml`: no match → the tool clicks Refresh (and its confirmation) in
  the game window; match → it shows the item details, pauses the refreshes,
  and buys the matched items the same way. The purchase confirmations decoded
  from the traffic check the matched items off, and the loop resumes on its
  own once the last one is bought. A `[limits]` threshold reached → it stops
  the session. `[actuator] dry_run = true` journals the planned clicks
  instead of sending them. The default build opens an **egui window**: a
  status bar (state, counters, one contextual Start/Stop button), a tabbed
  center (Shop table | Setup filter/limits editors — edits are
  session-only, `config.toml` is not rewritten), and a resizable session
  journal; there is no console beside the window — the journal carries
  every session line.

The relay starts **idle** (nothing is captured or forwarded): press **Start**
in the window (or type `start` in the console) when opening the shop to arm
the session, **Stop** when done.

## Distribution — a single exe

You ship **`arkyve-refresh-shop.exe` alone**. Both WinDivert runtime files ride
inside it (`include_bytes!`) and are self-extracted on first launch into
`%LOCALAPPDATA%\arkyve-refresh-shop\` — **nothing lands beside the exe**, so it
runs cleanly from the Desktop:

- **`WinDivert.dll`** (the official prebuilt library, vendored under
  `vendor/windivert/`) is **delay-loaded**: its import binds on the first
  WinDivert call, *after* the exe has extracted it and `LoadLibrary`'d it by full
  path from the app-data dir. Without delay-load the Windows loader would demand
  the DLL at process start — before any extraction could run — and abort with
  "WinDivert.dll not found".
- **`WinDivert64.sys`** (the signed kernel driver) is loaded by the DLL from that
  same directory. Windows loads a driver only from a file on disk, never from
  memory, so *some* file is unavoidable — but it stays in the hidden app-data
  dir, not in the user's face.

> **Delay-load needs a proper import lib.** The official release ships a "long"
> `WinDivert.lib` that `/DELAYLOAD` silently ignores (`LNK4199`). We regenerated
> it as an MSVC **short-import** lib from `vendor/windivert/WinDivert.def`
> (`lib.exe /DEF`). If you ever re-vendor it, run `cargo clean -p windivert-sys`
> — that crate copies the lib into its `OUT_DIR` and won't notice an in-place
> change.

> **Why not static?** `WINDIVERT_STATIC` compiles WinDivert's C from source and
> links it into the exe; that object corrupts the **release** build's stack-guard
> probing and the exe overflows its stack at startup (debug is unaffected). The
> official prebuilt DLL is a correct build, so we link against it instead.

> **License:** WinDivert is LGPL. The DLL is extracted (re-materialized as a file
> the user can replace with their own build) and its license is `vendor/windivert/LICENSE`
> — keep the license available in any redistribution.

## Requirements

- **End user**: Windows x64. Just the exe — double-click it. It carries a UAC
  manifest (`requireAdministrator`), so Windows shows the consent prompt on
  launch automatically (WinDivert loads a kernel driver, which needs admin); no
  "Run as administrator" needed. It self-extracts its runtime to `%LOCALAPPDATA%`
  on first run.
- **Build machine**: Rust >= 1.85 and the MSVC toolchain (`link.exe`). No C
  compiler needed — the DLL is prebuilt.

## Build

```sh
cargo build --release
```

WinDivert is linked dynamically against `vendor/windivert/` (set by
`WINDIVERT_PATH` in `.cargo/config.toml`). To build/test the pipeline without
the native backend:

```sh
cargo test --no-default-features
```

> **Dev note:** the default (WinDivert) build embeds a `requireAdministrator`
> manifest, so `cargo run` from a non-elevated shell fails with "requires
> elevation". Run it from an **elevated terminal**, launch the built exe
> directly (it prompts UAC), or use `--no-default-features` for pipeline work.

## Configuration

The app reads and writes its config at a per-user location, not beside the exe:

- **Windows:** `%APPDATA%\arkyve-refresh-shop\config.toml`
- **Other (dev):** `config.toml` in the working directory

The GUI owns this file — the Setup tab's Apply writes the edited sections back
to it — so it normally isn't hand-edited. On first run the bundled
`config.example.toml` (compiled into the exe) is written to that path, so a
real, commented, valid file is always there to inspect or edit; later runs
leave it untouched. Delete it to regenerate the example on the next launch.

| Key | Default | Purpose |
|-----|---------|---------|
| `game_port` | `3333` | Game server TCP port |
| `server_url` | `wss://ingest.arkyve.dev/refresh-shop` | Analysis server |
| `forward.server_to_client` | `true` | Forward responses (shop contents) |
| `forward.client_to_server` | `false` | Forward requests (context) |
| `[filter]` | matches everything | Item interest criteria (kinds, sets, substats, price) |
| `[limits]` | no limits | Session stop limits (refreshes, crystals, matches, duration) |
| `[actuator]` | live | `dry_run = true` journals planned clicks without sending input |

## Running

```sh
cargo run --release   # as administrator; opens the window (default features)
```

Control from the window: one contextual Start/Stop button in the status bar,
filter & limits editors under the Setup tab. Close the window to quit.

Console-only build (no window; `start`/`stop`/`[Enter]` on stdin, `Ctrl+C`
quits):

```sh
cargo run --release --no-default-features --features windivert-backend
```
