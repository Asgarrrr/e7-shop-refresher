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

## Distribution — exe + WinDivert.dll

WinDivert's user-mode library is the **official prebuilt `WinDivert.dll`**
(vendored under `vendor/windivert/`), linked **dynamically**. The
`WinDivert64.sys` driver is **embedded** in the exe (`include_bytes!`) and
extracted next to it on first launch. You ship **`arkyve-refresh-shop.exe` +
`WinDivert.dll`** (plus `WinDivert-LICENSE.txt`); the `.sys` rides inside the
exe. `cargo build` stages the DLL and license into `target/<profile>/` (via
`build.rs`), so that directory is ready to zip and ship as-is.

> The `.sys` is a kernel driver: Windows loads it from a file on disk (never from
> memory). The exe drops it beside itself — invisible to the user, and the already
> required admin rights are enough to write it.

> **Why not static?** `WINDIVERT_STATIC` compiles WinDivert's C from source and
> links it into the exe; that object corrupts the **release** build's stack-guard
> probing and the exe overflows its stack at startup (debug is unaffected). The
> official prebuilt DLL is a correct build, so we link against it instead.

> **License:** WinDivert is LGPL. We link it dynamically and ship
> `WinDivert-LICENSE.txt` beside the exe — keep it in any redistributed bundle.

## Requirements

- **End user**: Windows x64 + administrator rights at launch (WinDivert loads a
  kernel driver — UAC prompt on first run). `WinDivert.dll` beside the exe.
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

## Configuration

Copy `config.example.toml` to `config.toml` and adjust. Every key has a default;
a missing file falls back to the defaults.

| Key | Default | Purpose |
|-----|---------|---------|
| `game_port` | `3333` | Game server TCP port |
| `server_url` | `ws://127.0.0.1:3001/refresh-shop` | Analysis server |
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
