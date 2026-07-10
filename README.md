# Arkyve — Refresh Shop

Local relay for Epic Seven's Secret Shop. **Strictly passive and read-only**: it
observes a copy of the game's traffic, forwards the raw stream to an analysis
server, and displays the alerts it gets back. It automates nothing, sends no
data to the game, and never alters its communications.

## How it works

```
WinDivert SNIFF ─▶ parse IP/TCP ─▶ TCP reassembly ─▶ gate ─▶ WebSocket ─▶ server
   (blocking)                       (ordered/dedup)                  ▲         │
                                                                 alerts ◀──────┘
```

- **Capture**: WinDivert in `SNIFF` + `RECV_ONLY` mode yields a *copy* of the
  game-port TCP packets; the originals continue on their way intact.
- **Reassembly**: captured segments (possibly out of order or retransmitted) are
  recomposed into an ordered byte stream, per connection.
- **Forwarding**: the raw server → client stream is sent as-is to the analysis
  server. Decryption and interpretation happen **server-side** — the client
  decrypts nothing.
- **Display & control**: each decoded shop snapshot is fed to the refresh-loop
  controller, which checks it against the `[filter]` criteria from
  `config.toml`: no match → it advises a refresh (the relay stays passive —
  nothing is sent to the game); match → it alerts with the item details and
  pauses. The purchase confirmations decoded from the traffic check the
  matched items off, and the loop resumes on its own once the last one is
  bought. A `[limits]` threshold reached → it stops the session. The default
  build opens an **egui window** (status, shop table, session journal,
  Start/Stop buttons, live filter/limits editors — edits are session-only,
  `config.toml` is not rewritten); there is no console beside the window —
  the journal carries every session line.

The relay starts **idle** (nothing is captured or forwarded): press **Start**
in the window (or type `start` in the console) when opening the shop to arm
the session, **Stop** when done.

## Distribution — a single executable

WinDivert's user-mode code is **statically linked** into the exe, and the
`WinDivert64.sys` driver is **embedded** (`include_bytes!`) then extracted next
to the exe on first launch. You ship **one `.exe`** (e.g. a GitHub release): no
DLL or side files to bundle.

> The `.sys` is a kernel driver: Windows loads it from a file on disk (never from
> memory). The exe drops it itself — invisible to the user, and the already
> required admin rights are enough to write it.

## Requirements

- **End user**: Windows x64 + administrator rights at launch (WinDivert loads a
  kernel driver — UAC prompt on first run). Nothing else.
- **Build machine**: Rust >= 1.85 and the MSVC Build Tools (`cl.exe`) — static
  linking compiles WinDivert from its C sources.

## Build

```sh
cargo build --release
```

Static linking is enabled via `WINDIVERT_STATIC` in `.cargo/config.toml`. To
build/test the pipeline without the native backend (no MSVC required):

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

## Running

```sh
cargo run --release   # as administrator; opens the window (default features)
```

Control from the window: Start / Stop / Toggle buttons, filter & limits
editors. Close the window to quit.

Console-only build (no window; `start`/`stop`/`[Enter]` on stdin, `Ctrl+C`
quits):

```sh
cargo run --release --no-default-features --features windivert-backend
```
