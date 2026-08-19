# Why the capture backend is Npcap, and why a driver of our own was the wrong answer

Status: decided, 2026-08-18. Supersedes two earlier decisions recorded in this
same file: the PktMon migration attempted on 2026-08-16, and the WinDivert
decision written on 2026-08-17 to close the question for good. It did not close
it. Both of that version's load-bearing arguments were disproved by measurement
the next day, which is why this is a rewrite and not an amendment.

## Decision

The relay captures game traffic through **Npcap**, opened read-only on **every
adapter**, one handle and one kernel-side BPF filter (`tcp and src port 3333`)
each — `src port`, not `port`, so the client → server half is never copied.
Each capture thread strips its own adapter's link header, so what reaches the
pipeline is an IP packet — the same shape the previous backend delivered, from a
process with no special privilege.

`wpcap.dll` is resolved at **runtime** through `libloading`, never linked. A
static link would make the build need the Npcap SDK and — worse — would kill the
shipped exe *in the Windows loader, before `main`*, on any machine without Npcap
installed. Loaded by hand, "Npcap is not installed" is an ordinary error message
naming the download page.

We ship **no driver and embed nothing**. The player installs Npcap once, from
https://npcap.com, with default options.

## What the previous version of this document got wrong

Both arguments it used to reject Npcap were wrong, and each was wrong in a way
worth naming, because the same mistake is easy to make again.

### 1. It compared the wrong number

> *"That is the whole reason the backend is ~400 lines instead of ~2 300."*

The backend was ~400 lines. The **cost of the backend** was 3 274:

| File | Lines | What it was for |
| --- | ---: | --- |
| `src/capture/windivert.rs` | 845 | The tap itself, plus embedding, extraction, byte re-verification, DLL preload and DACL hardening of the extraction directory |
| `src/capture/elevate.rs` | 849 | Launching a second copy of the exe with the `runas` verb, and the pipe client |
| `src/broker.rs` | 1 041 | The elevated process: argv validators, pipe server, watchdog on the UI process |
| `src/capture/pipe.rs` | 539 | The frame protocol between the two |
| **Total** | **3 274** | plus a second process, a UAC prompt, and a delay-load link argument |

None of that was accidental complexity to be refactored away. Every line existed
because a kernel driver's handle had to be held by *something*, and holding it in
the process that also parses unauthenticated bytes off the wire was not
acceptable. The driver did not cost 400 lines. It cost an architecture.

The rule the earlier note stated for PktMon applies to its own decision: compare
what a choice *actually requires end to end*, not the module you would write to
front it.

### 2. It asserted that Npcap hits the 802.11 wall. It does not.

> *"its installer asks users to enable 'Support raw 802.11 traffic', which is the
> same wall, solved by paying a driver to normalise it."*

False, and backwards. Npcap's **default** installation binds above the Windows
Wi-Fi stack (NWIFI) and hands out **fake Ethernet** headers: the 802.11 framing
is already normalised, which is exactly what this pipeline needs. The "Support
raw 802.11 traffic" checkbox *creates* 802.11 framing — it switches the adapter
into a mode that reports real radio headers — so enabling it would have caused
the wall the note feared, not removed it.

That option exists for a different job: capturing traffic from **another
device**. Fribbels' Epic 7 Optimizer asks for it because it supports capturing an
emulator or a phone tethered to the PC's hotspot, where the PC is not the
endpoint. This tool captures the PC's own traffic, and never needs it.

Measured here, on the machine this runs on:

- Intel Wi-Fi 6 AX201, default Npcap install, `AdminOnly = 0`
- Link type reported: **`DLT_EN10MB`** (Ethernet), not 802.11
- 82 packets matched the filter, **82 parsed, 0 unparsed**
- Largest single capture: **48 870 bytes** — RSC coalescing, 32× the MTU, which
  is why the snaplen is 262 144 and not the wire MTU
- Whole pipeline verified from an **unelevated** process: capture → reassembly →
  uplink → analysis server → decoded shop snapshot → refresh job

The lesson is narrower than "measure things". The claim was inherited from
another project's install instructions and reasoned about by analogy with
PktMon's failure, which had genuinely been an 802.11 problem. A checkbox in
someone else's installer is not evidence about your own configuration.

## The PktMon post-mortem, which still stands

A migration to **PktMon** — the in-box Windows Packet Monitor API — was
implemented, debugged through four successive failures, and removed. It never
captured a single shop payload.

Every wall was a direct consequence of what PktMon is *for*. It is an
**observability tracer** built to answer "at which layer was my packet dropped?".
Using it as a capture tap means fighting its design intent on every axis.

| Wall | Cause | Symptom |
| --- | --- | --- |
| No packets at all | A session needs **data sources** attached; the constraint (what to keep) and the data source (where to listen) are separate concepts. `pktmon start --capture` attaches all components implicitly, the API does not. | Session opened, zero errors, zero packets, for nine minutes. |
| Massive loss | Attaching all 61 data sources reports the same packet at **every component and edge**, each appearance carrying its own `PktGroupId` — so deduplication on that key cannot collapse them. | 700–1 700 packets lost per burst; the callback queue could not drain. |
| Nothing decodable | Filtering to network-interface sources (61 → 5) fixed the flood, but NIC-level components report **link-layer** framing. On Wi-Fi that is 802.11 + LLC/SNAP. | `packet_type=2`, `admitted=0`. |
| Oversized packets refused | The realtime stream buffer caps at 9 000 bytes; RSC/LRO coalescing produces larger receives. | `ERROR_INSUFFICIENT_BUFFER` on a 9 064-byte packet. |

The API itself compounded this: no header, no import library, no public
bindings, and undocumented field lengths in
`PACKETMONITOR_DATA_SOURCE_SPECIFICATION`, which makes that struct unreadable
from Rust without guessing offsets.

**Npcap avoids all four**, and for the same structural reason WinDivert did: one
tap, one delivery per packet, framing normalised by the driver rather than by us.

### Amendment, 2026-08-19: two of those four walls were the API, not the data

Re-measured after the question was reopened, using `pktmon.exe` itself — Microsoft's
own consumer of the same NDIS machinery, with all the attachment logic already
correct — rather than the `PACKETMONITOR_*` API this project bound to.

**The bytes are there, driverless, in realtime.** With `--pkt-size 0 --comp nics`
and a `TCP port 443` filter, a verified 20 000 000-byte download produced **2 574
inbound TCP events, 2 535 of them carrying payload**, in `-m real-time`. Sample
frames read `162.159.140.220.443 > 192.168.1.17.2725 … length 1460`. This
contradicts nothing in the table above, but it does contradict the inference
drawn from it, that the in-box path cannot feed a capture backend.

Two corrections follow.

- **The 9 000-byte realtime cap is an API limit, not a data-path limit.** The
  same realtime run carried a **48 220-byte** frame without complaint. What
  failed with `ERROR_INSUFFICIENT_BUFFER` was the buffer the PktMon API hands its
  callback, not the stream.
- **48 KB coalesced frames are not an obstacle; they are the existing condition.**
  `capture/pcap/sys.rs`'s `SNAPLEN` comment records the Npcap backend measuring
  one of **48 870 bytes** on this same machine and sets the snaplen to 262 144 for
  exactly that reason. The RSC behaviour the post-mortem hit is what this pipeline
  already expects. (It also explains a discrepancy noticed during the re-run:
  20 MB across 2 535 events is ~7.9 KB per event, which is coalescing, not loss.)

Walls 1 and 2 — data-source attachment and per-component duplication — remain
real and remain properties of that API; `--comp nics` is what narrowed the second
here. Wall 3, 802.11 + LLC/SNAP framing on Wi-Fi, is real and unchanged: the
frames arrive with an 802.11 header and an LLC/SNAP shim in front of the IPv4
one. It is a parsing problem of the kind `capture/pcap/link.rs` already solves for
Ethernet, VLAN and QinQ, and the frames self-describe (`ethertype IPv4 (0x0800)`).

**What this does not establish.** No ETW consumer was written. All of the above
was observed through `pktmon.exe`, which may use a private path. The open
question is now narrow and worth stating precisely: *can a direct consumer of the
`Microsoft-Windows-NDIS-PacketCapture` provider get this same stream, at this same
fidelity, from Rust?* That is unexplored — the earlier migration went through the
capped API instead — and it is the only thing between this project and capture
with no install of any kind.

### Answered the same day: yes on one adapter, no with a VPN

A throwaway ETW consumer was written (Rust, `windows-sys` only, no new
dependency) and measured against a 20 000 000-byte ground truth.

**On a single-adapter machine it is excellent.** A plain `StartTraceW` +
`EnableTraceEx2` session on the provider, consumed with `ProcessTrace`, reached
**100.2 % of ground truth**, carried a **46 794-byte** frame with no
`ERROR_INSUFFICIENT_BUFFER`, parsed 100 % of frames, and reported zero lost
events. It beat `pktmon.exe`'s own numbers. Event 1001's layout is
`u32 MiniportIfIndex | u32 LowerIfIndex | u32 FragmentSize | u8 Fragment[] | 16
zero bytes`, measured rather than assumed. The medium keyword (`Native802.11` vs
`Ethernet802.3`) selects the parser before a byte is touched.

**Turn a VPN on and it collapses to 0.0 %.** Same probe, same ground truth: 5
inbound TCP frames totalling 72 bytes, against 18 384 inbound UDP frames. The
download went through the tunnel, so the physical adapter carried only encrypted
UDP, and **the VPN's virtual adapter never appeared in the session at all** —
one `(miniport 5, lower 5)` pair, the Wi-Fi card, and nothing else.

**So wall #1 is real, and the paragraph above was wrong to imply otherwise.**
"Enabling the provider is attaching the data source" held only because that
machine had one adapter and nothing else needed attaching. With two, the
constraint/data-source split this document described from the start is exactly
what bites. Recorded here rather than silently amended, because the earlier
reading was published and acted on.

Letting `pktmon.exe` do the attaching and consuming *its* realtime session as a
second reader does work under the VPN — **100.7 % of ground truth**, with the
tunnel's inner addresses (`10.2.0.2`) visible. It is rejected anyway, on what it
would rest on: PktMon's own event 160 carries an undocumented layout, and the
probe located the IPv4 header by scanning the blob for something that looked like
one. That is not a foundation for a pipeline that spends a player's gold, and it
would also mean spawning and supervising a CLI for the life of every session.
`LogBuffersLost` was 133 rather than 0 in that run.

**Conclusion: Npcap stays**, and the reason is now specific rather than general.
It is not that driverless capture is impossible — it demonstrably is possible,
at full fidelity, with no install. It is that the driverless path is blind
exactly where this product's users are most likely to be: behind a region VPN,
which is routine for gacha players.

## The transferable rule

> When replacing a component, ask whether the replacement is *designed for* the
> use case or merely *capable of* it. Observability tools optimise for
> completeness and attribution — show me everything, everywhere, labelled.
> Capture tools optimise for singularity and position — give me this, once,
> here. Swapping one for the other means fighting the design on every axis.

## The false premise that started all of it

WinDivert was abandoned for PktMon because a release build died with a stack
overflow, attributed to the crate. It was not the crate.

`WINDIVERT_STATIC` was **never enabled in the committed configuration**:
`windivert-sys` has `default = []` and only compiles the vendored C sources
under its `static` feature. The overflow came from an experiment with that
feature, not from the build that shipped. Verified after the fact: the release
binary's import table listed `WinDivert.dll` in the delay-load section only,
`OUT_DIR` contained no compiled objects, and the windowed release binary ran
without producing a crash log.

So roughly 2 300 lines of unsafe FFI were written to work around a bug that was
not there, and the backend was then rebuilt on the dependency that had been
blamed. The reflex that would have prevented it: **reproduce the failure on the
clean, committed configuration before concluding that the dependency is at
fault.**

## Alternatives evaluated

**Raw socket, `SIO_RCVALL`.** Attractive on paper: no install of any kind, no
service, no persistent state, and it delivers IP packets, so it escapes the
802.11 question entirely. Measured on the target machine: the socket arms on
Wi-Fi in both `RCVALL_ON` and `RCVALL_IPLEVEL` — Wi-Fi is *not* a blocker,
contrary to common claims, because `RCVALL_IPLEVEL` works above NDIS and needs no
promiscuous mode. But over ~60 000 packets it delivered **1 114 outbound TCP
packets and zero inbound TCP**, while inbound UDP flowed freely. This project
lives entirely on inbound. Documented sources suggest a Windows Firewall inbound
rule lifts the restriction; that was never confirmed, and the inbound-UDP
observation weakens the explanation. It would also require a persistent firewall
rule, which erodes the "installs nothing" argument that motivated the idea.
Rejected as unproven. It remains the only candidate that would beat Npcap on
install footprint.

**Re-measured 2026-08-19** on Windows 11 26200, Wi-Fi, all three firewall
profiles enabled, after the question was reopened. Same answer, and this time
with a ground truth rather than a packet count: `curl` wrote exactly 30 000 000
bytes to disk while the socket reported **zero inbound TCP packets and zero
inbound TCP bytes** — and 3 173 *outbound* TCP packets averaging **52 bytes**,
which is pure ACK size. The machine therefore received 30 MB and acknowledged
it, packet by packet, while the socket showed none of it. Two possible artefacts
were ruled out rather than argued away: a 64 MB receive buffer changed nothing,
and 2 962 inbound *UDP* packets arrived in the same window, so inbound delivery
works in general and it is TCP specifically that is withheld. `RCVALL_ON` and
`RCVALL_IPLEVEL` behave identically.

One fact that is new and belongs here for whoever picks this up if the firewall
question is ever settled: **`connect()` on the raw socket still filters by source
address under `SIO_RCVALL`.** Measured in both orderings (connect before the
ioctl and after): an unconnected socket saw 10 971 packets from 3 sources while a
connected one saw 10, from the connected peer only, with zero leakage. That
matters because the strongest objection to a raw-socket backend is not the
install footprint but the loss of the kernel-side BPF filter — this crate's
README promises that no other traffic is even copied, and a userspace filter
makes that false. `connect()` gives a kernel-side filter back, narrowed to one
peer, and `GetExtendedTcpTable` can supply that peer's address from the game's
PID without capturing anything. None of it rescues the design today, because the
packets it would filter never arrive.

**A local relay (proxy) the game connects through.** Never evaluated in the
earlier note, and it should have been, because it is the obvious answer for
anyone who has done this on a platform with a proxy setting. Rejected on what it
would make this tool *be*: a relay owns the game's socket. Read-only observation
becomes a man-in-the-middle by construction — every packet passes through our
code before reaching the game, so a bug, a stall or a crash in it is a bug, a
stall or a crash in the player's game session, and the README's promise that the
game's traffic is never altered stops being structurally true. Getting the game
to connect through it is its own problem: Epic Seven has no proxy setting, so the
only lever is a `hosts` entry, which is machine-wide, needs administrator rights
once to write, is policed by Defender's "tampering" heuristics, and **fails
closed** — a stale line left behind by a crash breaks the game until someone
edits a file they have never opened.

**In-process hooking (inject into the game and read the buffers).** Also never
evaluated. Rejected outright, and not on difficulty. Epic Seven ships Wellbia
**XIGNCODE3 / UNCHEATER**, whose `xhunter1.sys` kernel driver exists precisely to
detect foreign code inside the game process. Beyond the practical outcome — a
ban — this is a categorical line the rest of the design respects: observing a
copy of traffic the network hands us is not the same act as writing into another
process's memory, and no amount of care makes it the same act.

**PktMon.** See above.

**WinDivert.** The previous decision. It works, and it is genuinely elegant at
the tap: one WFP hook at the IP layer, no link framing, no adapter selection. It
is rejected on everything around the tap — a third-party signed kernel driver
shipped by us, registered as a service, flagged as riskware by several antivirus
products, requiring elevation and therefore requiring the whole broker
architecture above. Npcap gets IP-layer packets with none of it.

## Accepted trade-off

**Npcap must be installed by the user, and that cost does not go away.** It is a
separate download, a separate installer, a reboot-free but non-trivial step
between a player and a working tool, and it is the single strongest argument
anyone can make against this decision. It is accepted because the alternative was
not "no install" — it was "we install a kernel driver for you, silently, and your
antivirus tells you about it".

Two secondary points, both measured rather than assumed:

- The default install is what is needed. No option has to be changed, and in
  particular "Support raw 802.11 traffic" must be left **off**.
- Npcap's installer offers to restrict its driver to administrators
  (`AdminOnly`). Off by default, and the backend reads that registry value so it
  can tell "restricted to administrators" apart from "this machine has no
  adapters" instead of reporting an empty device list. The app happens to run
  elevated anyway — for the actuator, see `build.rs` — so a machine that did tick
  it still works.

## What would reopen this

- **Npcap becoming unavailable or unacceptable**: dropped from distribution,
  broken by a Windows release, or classified as unwanted software by mainstream
  antivirus the way WinDivert is.
- **A confirmed Windows Firewall rule making inbound TCP reach `SIO_RCVALL`.**
  That would be strictly better on install footprint than any driver, and the
  probe used to measure it is easy to rebuild. Note this is now the *only* thing
  standing in the way: the second objection — no kernel-side filter, and so no
  honest way to keep the README's "no other traffic is even copied" — was
  answered by the `connect()` measurement above. Whoever tests the firewall
  hypothesis should establish a ground truth first (download a known byte count
  and compare), because packet counts alone made this look like a quiet network
  twice.
- **A documented way to attach an arbitrary adapter to an ETW
  `NDIS-PacketCapture` session.** This is the whole of what is missing, and it is
  now a precise ask rather than a direction. The consumer side is solved and
  measured (100.2 % of ground truth, 46 KB frames, loss reporting, 100 % parse);
  what is not solved is making a VPN or other virtual adapter emit into a session
  we created, which `pktmon.exe` manages and a plain `EnableTraceEx2` does not.
  If that ever becomes documented, the backend is roughly 750 lines against
  ~1 100 of Npcap-specific code deleted, with no new dependency. Build it behind
  a feature flag beside Npcap and compare on one live session — with a VPN
  running — before choosing.
- **Evidence that the per-adapter fan-out does not scale** on a machine with many
  virtual adapters. Opening all of them buys the removal of every adapter
  heuristic; if it ever costs more than that, adapter selection comes back, not a
  different backend.

Reopening it because Npcap "needs an install" is not a new argument. That is the
trade-off, stated above, deliberately.
