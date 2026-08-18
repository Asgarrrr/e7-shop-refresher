# Arkyve — Refresh Shop

Local relay for Epic Seven's Secret Shop. The capture side is read-only: it
observes a copy of the game's traffic and forwards the raw stream to an
analysis server; the game's network traffic is never altered. On Windows the
tool drives the shop itself from the decoded snapshots — it refreshes and
buys matched items via click emulation.

## How it works

```
WinDivert SNIFF ─▶ parse IP/TCP ─▶ TCP reassembly ─▶ gate ─▶ WebSocket ─▶ server
   (passive)                        (ordered/dedup)                  ▲         │
                                                              snapshots ◀──────┘
```

- **Capture**: WinDivert in `SNIFF` + `RECV_ONLY` mode yields a *copy* of the
  game-port TCP packets; the originals continue on their way intact. `SNIFF`
  is what makes the tap a copy rather than a diversion, and `RECV_ONLY` makes
  the handle physically incapable of sending — nothing can be injected or
  modified, by this code or any later addition to it.
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
  center (Shop table | Setup filter/limits editors — Apply is applied live
  and written back to `config.toml`), and a resizable session
  journal; there is no console beside the window — the journal carries
  every session line.

The relay starts **idle** (nothing is captured or forwarded): press **Start**
in the window (or type `start` in the console) when opening the shop to arm
the session, **Stop** when done.

## What gets installed on your machine — read this before launching

Capture needs to see packets before Windows hands them to the game, and the
only supported way to do that is a kernel-mode driver. **WinDivert** is that
driver. Being upfront about it:

- **A signed kernel driver is registered as a Windows service.** On the first
  capture, `WinDivert.dll` registers `WinDivert64.sys` (WinDivert 2.2.2, signed
  by its author, Basil) as a kernel service named `WinDivert` and starts it. It
  is registered with start type **disabled** and marked for deletion, so it
  never starts on its own and never comes back at boot — it only ever runs
  because this tool asked for it. Check for yourself:

  ```powershell
  sc.exe qc WinDivert      # START_TYPE : 4  DISABLED
  sc.exe query WinDivert   # RUNNING only while a capture handle is open
  ```

  The driver can stay resident for a while after the tool exits, until Windows
  drops the last kernel reference; it is gone after a reboot in any case.
- **Administrator rights are required**, for that reason alone. The exe carries
  a `requireAdministrator` manifest, so Windows shows the UAC prompt on launch
  rather than letting the first capture fail with a puzzling error.
- **The mode is `SNIFF` + `RECV_ONLY`.** The driver gives the tool a *copy* of
  matching packets and the originals travel on untouched; the handle cannot
  send, so nothing is injected, dropped or rewritten. The filter is narrow by
  default — TCP with source port `game_port` (3333) — so no other traffic on
  the machine is even copied.
- **Some antivirus products flag WinDivert as *riskware* / *hacktool***
  (`HackTool:Win32/WinDivert`, `RiskWare.WinDivert`, and similar). This is a
  category judgement, not a detection of malicious behaviour: WinDivert is a
  legitimate, widely used, signed packet-capture library, and the same driver is
  bundled with plenty of ordinary VPN and firewall tools. **Expect an alert on
  first launch**, and expect to have to allow it. If you would rather not, do
  not run this tool.
- **What leaves your machine**: only the reassembled game-port byte stream, sent
  over TLS to the configured `server_url`. Nothing else on the network is read,
  and nothing is uploaded about your machine.

The tool is never a proxy and never owns the game's socket. Closing it releases
the handle and the driver; the game's TCP connection continues normally.

## Distribution — a single exe

You ship **`arkyve-refresh-shop.exe` alone**. Both WinDivert runtime files ride
inside it (`include_bytes!`) and are self-extracted on first launch into
`%LOCALAPPDATA%\arkyve-refresh-shop\runtime\` — **nothing lands beside the exe**,
so it runs cleanly from the Desktop:

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

Each file is written only when missing or different, through a temp file and an
atomic rename, and re-verified byte for byte immediately before it is loaded: a
runtime file that exists but does not match the embedded copy is refused rather
than loaded into an elevated process. If extraction fails outright the app stops
with a message naming the file — it never falls back to whatever happens to be
on disk.

The `runtime\` leaf is its own directory on purpose. It is locked down to
administrators and SYSTEM so that no non-elevated process can plant a file where
an elevated one is about to load a kernel driver from — and that lock is
inheritable, so it must not sit on `%LOCALAPPDATA%\arkyve-refresh-shop\` itself,
which also holds `logs\` and `crash.log` and has to stay writable without
administrator rights. Versions that extracted into the root are migrated
automatically on the first elevated launch (permissions restored, stale files
removed); that one run's log file is lost, and the next run's log says so.

> **Delay-load needs a proper import lib.** The official release ships a "long"
> `WinDivert.lib` that `/DELAYLOAD` silently ignores (`LNK4199`). We regenerated
> it as an MSVC **short-import** lib from `vendor/windivert/WinDivert.def`
> (`lib.exe /DEF`). If you ever re-vendor it, run `cargo clean -p windivert-sys`
> — that crate copies the lib into its `OUT_DIR` and won't notice an in-place
> change.

> **Why not static?** `WINDIVERT_STATIC` compiles WinDivert's C from source and
> links it into the exe; that object corrupts the **release** build's stack-guard
> probing and the exe overflows its stack at startup (debug is unaffected). The
> official prebuilt DLL is a correct build, so we link against it instead — see
> `.cargo/config.toml`. Verify with
> `dumpbin /dependents target\release\arkyve-refresh-shop.exe`: `WinDivert.dll`
> must appear under *delay load dependencies*, and nowhere else.

> **License:** WinDivert is LGPL. The DLL is extracted (re-materialized as a file
> the user can replace with their own build) and its license is `vendor/windivert/LICENSE`
> — keep the license available in any redistribution.

## Requirements

- **End user**: Windows x64. Just the exe — double-click it. It carries a UAC
  manifest (`requireAdministrator`), so Windows shows the consent prompt on
  launch automatically (WinDivert loads a kernel driver, which needs admin); no
  "Run as administrator" needed. It self-extracts its runtime to `%LOCALAPPDATA%`
  on first run.
- **Build machine**: Rust >= 1.92 and the MSVC toolchain (`link.exe`). No C
  compiler needed — the DLL is prebuilt.

## Build

```sh
cargo build --release
```

`rust-toolchain.toml` selects Rust 1.92.0 with Clippy and rustfmt for
reproducible local checks. The canonical quality command on Windows, macOS and
Linux is:

```sh
just verify
```

It checks formatting plus the platform-independent Clippy and test lanes. On
Windows, `just backends` adds the two Windows-only lanes: the capture backend on
its own, and the shipped default features (`windivert-backend,gui,actuator`),
each both linted and tested. CI repeats all of it on Rust 1.92.0 and current
stable, then builds the default-feature Windows release on stable.

WinDivert is linked **dynamically** against `vendor/windivert/` (set by
`WINDIVERT_PATH` in `.cargo/config.toml`), never statically — see *Why not
static?* above.

### Capture backend

One backend sits behind the `PacketSource` abstraction:

| Feature | Layer it reads | Needs | Status |
|---------|----------------|-------|--------|
| `windivert-backend` | IP packets | Kernel driver + admin | **Default.** Indifferent to the adapter — works on WiFi. |

Reading IP-layer copies is what makes it adapter-independent: a NIC-level tap
hands out link-layer frames, which on a WiFi adapter are 802.11 and are not
decoded here. Without the feature, the pipeline still builds and tests and
capture fails with a clear message instead of panicking:

```sh
cargo test --no-default-features
```

> **Dev note:** the native backend embeds a `requireAdministrator` manifest,
> so `cargo run` from a non-elevated shell fails with "requires elevation". Run
> it from an elevated terminal, launch the built exe directly (it prompts UAC),
> or use `--no-default-features` for pipeline work.

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
| `capture.buffer_size` | `65535` | Receive buffer per packet; raised to 65575 (the WinDivert maximum) |
| `capture.filter` | derived | Manual WinDivert filter override; must mention `game_port` |
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

## Troubleshooting

The windowed build has no console: nothing is printed anywhere you can read.
Everything it knows is written to two files under
`%LOCALAPPDATA%\arkyve-refresh-shop\`:

| File | Contents |
|------|----------|
| `logs\arkyve-refresh-shop.<date>.log` | Full session log, rotated daily, last 5 days kept. Startup line (version, features, config path), capture progress, server link state, and every player-facing journal line. |
| `crash.log` | One appended record per panic: thread, location, message, backtrace. Only written when something actually crashes. |

If `%LOCALAPPDATA%` is unset (non-Windows dev machines), both fall back to the
system temp directory.

**When reporting a problem, send the most recent `logs\*.log` file** (plus
`crash.log` if it exists). The log never contains the server URL's credentials
— userinfo and query string are stripped before anything is written.

Reading it yourself:

- `arkyve-refresh-shop starting` missing → the app never got to run; check the
  UAC prompt was accepted, and that no antivirus quarantined the exe or the
  extracted `WinDivert.dll` / `WinDivert64.sys`.
- No `WinDivert capture open` line → the driver never loaded. Either the process
  is not elevated, or the extraction into
  `%LOCALAPPDATA%\arkyve-refresh-shop\runtime\` failed — the error that follows
  names the file.
- `WinDivert capture open` but no `first server-to-client segment admitted` →
  the tap is running and the game server's traffic never matched. Check the
  `filter=` value on that same line against the port the game actually uses.
- No `capture progress` line for a whole session → nothing is reaching the
  pipeline on `game_port`; same causes as above. Set
  `RUST_LOG=arkyve_refresh_shop=debug` to get the `WinDivert capture funnel`
  line, which separates "no packet was ever delivered" from "packets arrived
  and the parser rejected them".
- `session heartbeat` lines with a growing `since_last_shop_s` → the pipeline
  is alive but no shop is arriving; compare `gate_armed` (is the watch armed?)
  with the `server link down` lines (is the server reachable?).
- No `session heartbeat` at all → the session loop itself is gone; look for
  `session aborted` just before.

Raise or narrow the verbosity with `RUST_LOG`, e.g. `RUST_LOG=journal=info`
for the player-facing lines only, or `RUST_LOG=arkyve_refresh_shop=trace`.
