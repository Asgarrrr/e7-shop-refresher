# Why the capture backend is WinDivert, and not PktMon

Status: decided, 2026-08-18. Supersedes the PktMon migration attempted the day before.

## Decision

The relay captures game traffic through **WinDivert**, opened in
`sniff` + `recv_only` mode at the **network (IP) layer**.

A migration to **PktMon** — the in-box Windows Packet Monitor API — was
implemented, debugged through four successive failures, and removed. It never
captured a single shop payload. This note exists so that nobody, human or
agent, reopens "we could drop the kernel driver" without first reading why it
did not work.

## The motivation was a false premise

WinDivert was abandoned because a release build died with a stack overflow,
attributed to the crate. It was not the crate.

`WINDIVERT_STATIC` was **never enabled in the committed configuration**:
`windivert-sys` has `default = []` and only compiles the vendored C sources
under its `static` feature. The overflow came from an experiment with that
feature, not from the build that shipped. Verified after the fact: the release
binary's import table lists `WinDivert.dll` in the **delay-load** section only,
`OUT_DIR` contains no compiled objects, and the windowed release binary runs
without producing a crash log.

So roughly 2 300 lines of unsafe FFI were written to work around a bug that was
not there. The reflex that would have prevented it: **reproduce the failure on
the clean, committed configuration before concluding that the dependency is at
fault.**

A CI step now fails the build if `WinDivert.dll` ever stops being delay-loaded —
a static import compiles fine but makes every launch die with "WinDivert.dll not
found", which no compile-time check would catch.

## Why PktMon could not do the job

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

**WinDivert avoids all four by construction.** It hooks the Windows Filtering
Platform at a single point, at the IP layer: each packet arrives once, already
stripped of its link framing, whatever the medium — Ethernet, Wi-Fi, VPN. That
is the whole reason the backend is ~400 lines instead of ~2 300.

## The transferable rule

> When replacing a component, ask whether the replacement is *designed for* the
> use case or merely *capable of* it. Observability tools optimise for
> completeness and attribution — show me everything, everywhere, labelled.
> Interception tools optimise for singularity and position — give me this, once,
> here. Swapping one for the other means fighting the design on every axis.

## Alternatives evaluated

**Raw socket, `SIO_RCVALL`.** Attractive on paper: no driver, no service, no
persistent state, and it delivers IP packets, so it escapes the 802.11 problem
entirely. Measured on the target machine: the socket arms on Wi-Fi in both
`RCVALL_ON` and `RCVALL_IPLEVEL` — Wi-Fi is *not* a blocker, contrary to common
claims, because `RCVALL_IPLEVEL` works above NDIS and needs no promiscuous mode.
But over ~60 000 packets it delivered **1 114 outbound TCP packets and zero
inbound TCP**, while inbound UDP flowed freely. This project lives entirely on
inbound. Documented sources suggest a Windows Firewall inbound rule lifts the
restriction; that was never confirmed, and the inbound-UDP observation weakens
the explanation. It would also require a persistent firewall rule, which erodes
the "installs nothing" argument that motivated the idea. Rejected as unproven.

**Npcap via the `pcap` crate.** The smallest backend of all (~100–150 lines, BPF
filter `tcp and port 3333`, kernel-side filtering, link framing normalised).
This is what Fribbels' Epic 7 Optimizer does through scapy — and its installer
asks users to enable "Support raw 802.11 traffic", which is the same wall,
solved by paying a driver to normalise it. Rejected only because it needs a
**separate** user install; kept as the fallback if WinDivert becomes untenable.
Npcap has a better reputation than WinDivert with security software, and its
installer can restrict access to administrators.

## Accepted trade-off

WinDivert installs a signed kernel driver as a service and requires elevation.
Several antivirus products classify it as riskware, because the library can also
divert, modify and reinject traffic. This relay uses none of that: the handle is
opened `sniff` + `recv_only`, so packets are copied, never altered, dropped or
reinjected — which is what backs the README's promise that the game's traffic is
untouched. The `README` states plainly what is installed and why, so players
learn it before an antivirus prompt rather than after.

## What would reopen this

- WinDivert being blocked outright by Windows or by common antivirus policy.
- A confirmed Windows Firewall rule making inbound TCP reach `SIO_RCVALL` —
  that would be strictly better than a kernel driver, and the probe used to
  measure this is easy to rebuild.
- A PktMon revision exposing a documented single-tap capture mode at the IP
  layer. As of Windows 11 26200, no such mode exists.

Anything else is re-treading this note.
