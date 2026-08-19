# Arkyve — Refresh Shop

Local relay for Epic Seven's Secret Shop. The capture side is read-only: it
observes a copy of the game's traffic and forwards the raw stream to an
analysis server; the game's network traffic is never altered. On Windows the
tool drives the shop itself from the decoded snapshots — it refreshes and
buys matched items via click emulation.

## How it works

```
Npcap tap ─▶ parse IP/TCP ─▶ TCP reassembly ─▶ gate ─▶ WebSocket ─▶ server
 (passive)                   (ordered/dedup)                  ▲         │
                                                       snapshots ◀──────┘
```

- **Capture**: an Npcap read-only tap on every network adapter yields a *copy*
  of the game-port TCP packets; the originals continue on their way intact. The
  tool is never a proxy and never owns the game's socket — nothing can be
  injected, dropped or rewritten, by this code or by any later addition to it,
  because a capture handle physically cannot send. The kernel-side filter is
  fixed (`tcp and src port 3333`, built from `game_port`), so no other traffic
  on the machine is even copied. The `src` narrows it further: only what the
  game *server* sends is copied, and the client → server half never leaves the
  driver.
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

## What you need to install, and why the app asks for administrator

Two things before you launch it: you install Npcap yourself, and Windows will
ask for administrator.

### You install Npcap, once. We ship no driver.

Capture needs to see the game's packets, and on Windows that means a packet
capture driver. **We do not ship one.** Nothing is embedded in the exe, nothing
is extracted, no service is registered by this tool. Instead the app uses
**Npcap** (the standard Windows packet-capture library, the one Wireshark
installs), which you install yourself:

1. Download and run
   **https://dev-libs.wireshark.org/windows/packages/Npcap/npcap-1.88.exe**

   That is Wireshark's build mirror, not npcap.com, and the swap is measured
   rather than preferred: npcap.com answers in 6–9 seconds from here and its own
   installer URL failed outright at 19 seconds with the TLS handshake never
   completing, while the mirror answers in 0.27 s and delivers the 1.3 MB in
   0.81 s. The file is the genuine installer, not a repackage —
   Authenticode-signed `CN=Nmap Software LLC`, issued by DigiCert, valid to 2027
   and timestamped, which you can check yourself with
   `Get-AuthenticodeSignature npcap-1.88.exe`. **https://npcap.com** remains the
   canonical home if you would rather go through it.
2. **Keep the default options.** In particular:
   - Leave *"Restrict Npcap driver's access to Administrators"* **unchecked**
     (that is the default). The app works either way, but unchecking it is what
     keeps the capture itself unprivileged.
   - Leave *"Support raw 802.11 traffic (and monitor mode)"* **unchecked** (also
     the default). Some other capture tools ask you to enable it; this one does
     not need it, and it does not help here. On a normal install Npcap already
     hands out ordinary Ethernet framing on Wi-Fi — measured on this project's
     Wi-Fi adapter: 82 packets captured, 82 parsed, 0 rejected.
3. That is all. No reboot, and nothing to configure in this app.

If Npcap is missing, the app still starts and tells you so with a message naming
the download page — it does not crash and it does not fail silently. In the
window the address is a link; installing Npcap then needs the app restarted.

**What leaves your machine**: only the reassembled game-port byte stream, sent
over TLS to the configured `server_url`. Nothing else on the network is read,
and nothing is uploaded about your machine.

### Administrator is for the clicking, not the capture

Windows shows a UAC prompt when you launch the exe. Capture needs no privilege
of its own; the clicking does.

Epic Seven is launched by STOVE, and STOVE runs as administrator. Windows will
not let an ordinary program send mouse clicks to a window belonging to an
administrator program — it refuses them silently, with no error the program can
even see. Since this tool's whole job is to click Refresh and Buy in the game
window, it has to run at the level the game does. There is no fix from the
game's side: STOVE is the launcher Epic Seven ships with, and it always
elevates.

So **approve the prompt**, or the refresh loop will do nothing at all. (An
earlier version of this file said the opposite. It was written when capture
needed a kernel driver and clicking was an afterthought.)

## Distribution — a single exe

You ship **`arkyve-refresh-shop.exe` alone**. It embeds nothing, extracts
nothing, and writes nothing beside itself, so it runs cleanly from the Desktop.
The only files it ever creates are its config under `%APPDATA%` and its logs and
crash log under `%LOCALAPPDATA%` (see *Troubleshooting*).

`wpcap.dll`, Npcap's library, is resolved **at runtime**: by plain name and
then by full path in `C:\Windows\System32\Npcap\`. It is deliberately not linked
at build time: a linked import would make Windows demand the DLL before `main`
runs, so the exe would die in the loader, with no message at all, on every
machine without Npcap. Resolved by hand, "Npcap is not installed" is just a line
in the window's journal.

> **Upgrading from a version that used WinDivert?** Those builds extracted
> `WinDivert.dll`, `WinDivert64.sys` and a licence file into
> `%LOCALAPPDATA%\arkyve-refresh-shop\`, and locked that folder down to
> administrators — which also made `logs\` unwritable for anything unelevated.
> The first launch of this version deletes those files and restores the folder's
> normal permissions by itself. There is nothing to do, and nothing to uninstall:
> the WinDivert *service*, if it is still registered, was registered disabled and
> marked for deletion and is gone after a reboot.

## Requirements

- **End user**: Windows x64, plus **Npcap** installed once with default options
  (see above). Then just the exe — double-click it and approve the UAC prompt.
  The exe carries a `requireAdministrator` manifest, so the prompt appears at
  launch, every launch; it is what lets the tool click in the game's window.
- **Build machine**: Rust >= 1.92 and the MSVC toolchain (`link.exe`). No C
  compiler and no SDK: `wpcap.dll` is resolved at runtime, so the build needs
  nothing from Npcap and CI compiles and tests this on runners that do not have
  it installed.

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
its own, and the shipped default features (`pcap-backend,gui,actuator`), each
both linted and tested. CI repeats all of it on Rust 1.92.0 and current stable,
then builds the default-feature Windows release on stable and checks that the
`requireAdministrator` manifest is still embedded in it.

### Capture backend

One backend sits behind the `PacketSource` abstraction:

| Feature | Layer it reads | Needs | Status |
|---------|----------------|-------|--------|
| `pcap-backend` | IP packets (link header stripped per adapter) | Npcap installed by the user | **Default.** Every adapter at once — works on Wi-Fi, survives a Wi-Fi/Ethernet switch. |

Every adapter is opened rather than one being chosen, which removes every
"which interface carries the game?" heuristic; the kernel-side BPF filter means
an idle adapter costs a parked thread and nothing per packet. Without the
feature, the pipeline still builds and tests, and capture fails with a clear
message instead of panicking:

```sh
cargo test --no-default-features
```

[`docs/capture-backend-choice.md`](docs/capture-backend-choice.md) explains why
Npcap replaced a driver of our own, including the two measurements that
overturned the previous decision.

## Configuration

The app reads and writes its config at a per-user location, not beside the exe:

- **Windows:** `%APPDATA%\arkyve-refresh-shop\config.toml`
- **Other (dev):** `config.toml` in the working directory

The GUI owns this file: the Setup tab's Apply writes the edited sections back
to it, so it normally isn't hand-edited. On first run the bundled
`config.example.toml` (compiled into the exe) is written to that path, so a
real, commented, valid file is always there to inspect or edit; later runs
leave it untouched — with one exception: a file still carrying the retired keys
below has them deleted, once, at the startup that warns about them. Delete the
file to regenerate the example on the next launch.

| Key | Default | Purpose |
|-----|---------|---------|
| `game_port` | `3333` | Game server TCP port |
| `server_url` | `wss://ingest.arkyve.dev/refresh-shop` | Analysis server |
| `forward.server_to_client` | — | **Retired.** Still parsed so older files keep loading, then removed from your `config.toml` at the next startup; ignored (the server → client stream is the only one captured) |
| `forward.client_to_server` | — | **Retired.** Still parsed so older files keep loading, then removed from your `config.toml` at the next startup; ignored (the client → server stream is never captured) |
| `capture.buffer_size` | — | **Retired.** Still parsed so older files keep loading, then removed from your `config.toml` at the next startup; ignored (the backend sizes its own buffer) |
| `capture.filter` | — | **Retired.** Still parsed so older files keep loading, then removed from your `config.toml` at the next startup; ignored (the backend builds its own filter from `game_port`) |
| `[filter]` | matches everything | Item interest criteria (kinds, sets, substats, price) |
| `[limits]` | no limits | Session stop limits (refreshes, crystals, matches, duration) |
| `[actuator]` | live | `dry_run = true` journals planned clicks without sending input |

## Running

```sh
cargo run --release   # opens the window (default features)
```

Windows asks for consent at launch (see *why the app asks for administrator*
above), and the window opens straight after.

Control from the window: one contextual Start/Stop button in the status bar,
filter & limits editors under the Setup tab. Close the window to quit.

Console-only build (no window; `start`/`stop`/`[Enter]` on stdin, `Ctrl+C`
quits):

```sh
cargo run --release --no-default-features --features pcap-backend
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

- `arkyve-refresh-shop starting` missing → either the app never got to run
  (check that no antivirus quarantined the exe, and that the UAC prompt at launch
  was approved rather than dismissed), or it ran fine and could not create the
  log file at all. The second case looks identical from here, because there is no
  file to look in: if `%LOCALAPPDATA%\arkyve-refresh-shop\logs` is not writable
  (a leftover admins-only ACL from an old build, antivirus, a roaming-profile
  policy, a full disk) logging falls back to a console this build does not have.
  A panic report does have a second candidate, `arkyve-crash.log` in the system
  temp directory, so one of those with no session log beside it means the app
  definitely ran.
- `wpcap.dll loaded` missing → Npcap is not installed, or its driver is not
  running. The error that follows says which, and names https://npcap.com — in
  the window it is a link you can click. **Installing Npcap then needs the app
  restarted**: the capture tap is opened once, at startup, so a window left open
  through the install stays dead. If the error says Npcap enumerated no capture
  device at all on a machine that clearly has one, the driver is not running;
  reinstalling Npcap with default options is the fix.
- `adapter opened and filtered` on no adapter at all → every adapter was
  refused, and the error lists each one with its reason. An install that ticked
  *"Restrict Npcap driver's access to Administrators"* is the usual cause, and
  the app names that case explicitly.
- Adapters open but no `first server-to-client segment admitted` → the tap is
  running and the game server's traffic never matched. Check `game_port` against
  the port the game actually uses.
- No `capture progress` line for a whole session → nothing is reaching the
  pipeline on `game_port`; same causes as above. Set
  `RUST_LOG=arkyve_refresh_shop=debug` to get the `capture funnel` line, which
  separates "no packet was ever delivered" from "packets arrived and the parser
  rejected them".
- `the capture driver dropped packets` → the kernel ring overflowed; the byte
  stream has a hole and the session resyncs by itself. Frequent enough to be
  noticeable means the machine could not keep up with the capture.
- `session heartbeat` lines with a growing `since_last_shop_s` → the pipeline
  is alive but no shop is arriving; compare `gate_armed` (is the watch armed?)
  with the `server link down` lines (is the server reachable?).
- No `session heartbeat` at all → the session loop itself is gone; look for
  `session aborted` just before.
- `the application window could not be created` → the relay started but the
  window never opened, so nothing after it in this list will be there. The `error`
  field names the cause; the usual ones are a machine with no usable OpenGL
  context (a stale or generic display driver), an RDP or remote session, and a
  service account with no desktop. The log ends right there because a windowed
  build has no console for the same message to go to.
- `startup failed` followed by `the error window could not be shown either` →
  the app refused to start (a bad `config.toml`, no capture backend) *and* could
  not put the reason on screen, so the player saw a double-clicked exe do
  nothing at all. The `startup failed` record above it carries the real cause.

Raise or narrow the verbosity with `RUST_LOG`, e.g. `RUST_LOG=journal=info`
for the player-facing lines only, or `RUST_LOG=arkyve_refresh_shop=debug` for the
capture internals. `debug` is as detailed as it gets: nothing in the app logs at
`trace`, so asking for that level changes nothing.

Narrowing all the way to `RUST_LOG=warn` is safe for triage: the lines that say
the product stopped doing its job (`session aborted` and `actuator: … stopping
the loop`) are recorded at `error`, and `server link down` and an aborted job at
`warn`, so they survive. Only the routine player narration is dropped.
