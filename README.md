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
- **Display**: server messages (shop snapshot, alerts) are rendered in the
  console.

The **Shop Watch** switch (on by default) stops forwarding while the player is
not in the shop.

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

## Running

```sh
cargo run --release   # as administrator
```

Runtime commands: `[Enter]` toggles Shop Watch, `on`, `off`, `Ctrl+C` to quit.
