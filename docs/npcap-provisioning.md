# How a player gets Npcap, and why this app will never install it for them

Status: decided, 2026-08-19. Follows `docs/capture-backend-choice.md`, which
chose Npcap and recorded "Npcap must be installed by the user" as the accepted
trade-off — then said nothing about what the app should *do* about it. This
answers that, and the answer is settled by a licence rather than by engineering.

## Decision

The app **never ships, downloads, extracts, or installs Npcap**, and never will
without written permission from the Nmap Project. What it does instead is detect
the absence properly and hand the player something they can act on.

Landed: a message that leads with the one step rather than with two DLL paths,
and a clickable link to the download page in the window that is the only surface
this build has. Both are exercised — see *What happens today*.

Deferred, deliberately: the re-probe, so that installing Npcap does not need the
app relaunched. It waits on a measurement, not on a decision.

Every richer option on the list is either forbidden by the licence, or is
worse than this one on grounds that have nothing to do with the licence.

## The licence decides this, and it is not close

Retrieved 2026-08-19 from
<https://raw.githubusercontent.com/nmap/npcap/master/LICENSE> (header:
"copyright (c) 2013-2025 by Nmap Software LLC"), the canonical text also
reachable from <https://npcap.com>. Four clauses matter.

**1. Redistribution is not granted.** From the preamble, second paragraph:

> Even though Npcap source code is publicly available for review, it is not
> open source software and may not be redistributed or used in other software
> without special permission from the Nmap Project. The standard (free) version
> is usually limited to installation on five systems.

**2. The silent installer is a product we do not have.** Same preamble, and
again at the end of LICENSE GRANT:

> Both of these licenses include updates and support as well as a warranty.
> Npcap OEM also includes a silent installer for unattended installation.

> Users wishing to redistribute Npcap or exceed the usage limits imposed by
> this free license or benefit from commercial support and features such as a
> silent installer should contact sales@nmap.com to obtain an appropriate
> commercial license agreement.

`/S` is not a flag the free installer refuses. It is a **different build of the
installer**, sold separately. There is no unattended mode to reach from here.

**3. RESTRICTIONS ON TRANSFER**, in full:

> Without first obtaining the express written consent of the Nmap Project, you
> may not assign your rights and obligations under this Agreement, or
> redistribute, encumber, sell, rent, lease, sublicense, or otherwise transfer
> your rights to the Software Product.

**4. And then the licensor writes this project's answer for it**, in the same
preamble:

> Free and open source software producers are also welcome to contact us for
> redistribution requests. However, we normally recommend that such authors
> instead ask your users to download and install Npcap themselves. It will be
> free for them if they need 5 or fewer copies.

That is not a loophole being read generously. It is the copyright holder
describing, unprompted, the arrangement it prefers for exactly this shape of
project — free, non-commercial, on GitHub, one Npcap per player. The decision
above is that sentence, implemented.

The alternative is priced. <https://npcap.com/oem/redist.html> lists the OEM
Redistribution License at **$79,980 (Enterprise) / $59,980 (Mid-Sized) /
$39,980 (Small & Startup)** perpetual, and answers "Are there any limits or
extra fees based on how many copies of Npcap OEM we redistribute or install?"
with "No." For a hobby tool with no revenue that is not a number to negotiate;
it is a number that ends the conversation.

### The one ambiguity, stated rather than papered over

Read at its widest, "may not be redistributed **or used in other software**
without special permission" would cover what `src/capture/pcap/sys.rs` already
does: `LoadLibrary` on the player's own `wpcap.dll`. Three things weigh against
that reading, and none of them is a written permission:

- the very next paragraph recommends that FOSS authors "ask your users to
  download and install Npcap themselves", which is only coherent if the user's
  own copy being driven by someone else's program is the intended arrangement;
- the whole Windows libpcap ecosystem — every tool that is not Wireshark or
  Nmap — works this way, and the free licence's five-system cap is written in
  terms of *installs on the player's machine*, not in terms of which program
  opens the handle;
- the licence's ACCEPTANCE clause binds the person who installed it, which on
  this arrangement is the player, who chose to.

It is still not permission in writing. If it ever matters, the licence names the
address (sales@nmap.com) and invites the request. Recorded here so that nobody
later mistakes "we reasoned about it" for "we were told yes".

## Alternatives evaluated

### Bundle the installer beside the exe, or embed it — rejected on clause 1

Plainly redistribution, with or without the exe. Nothing to argue.

It is also worth naming what it would cost even if it were legal, because this
codebase has the receipt: embedding a binary and extracting it at runtime is the
exact footprint `src/migrate.rs` exists to clean up after. That module deletes
three extracted files and rewrites a **protected DACL** a WinDivert build left
on `%LOCALAPPDATA%\arkyve-refresh-shop\` — a DACL that silently cost every
unelevated run its log file until someone measured it. The README's
"You ship `arkyve-refresh-shop.exe` alone. It embeds nothing, extracts nothing"
is a property that was paid for once already.

### Silently install it for the player — rejected on clause 2, twice over

There is no free silent installer to invoke, so this is not a policy choice we
are declining, it is a capability we do not have. And if we did: the EULA's
ACCEPTANCE clause conditions the licence on the player agreeing to it. An app
that clicks through a EULA on a player's behalf, from an elevated process, is
doing something categorically worse than saving them a download.

### Download the official installer at runtime and launch it — rejected

The strongest of the losing options, and the only one that needed an argument
rather than a clause. The bytes would come from npcap.com, so nothing is
redistributed; the installer would run interactively, so the player still clicks
"I Agree". Rejected on five grounds, in descending order of weight:

1. **It is the practice the ecosystem already treats as out of bounds.**
   `microsoft/winget-cli#5612` names <https://npcap.com/oem/> as its example of
   licences that "specify that the download and / or installation are only free
   when users visit and manually download the package from their respective
   publisher and do not allow for automated downloads directly from their
   servers and/or silent installs". Reading clause 4 — "ask your users to
   download and install Npcap themselves" — as compatible with the app doing the
   downloading is reading past the word *themselves*.
2. **It buys almost nothing.** The installer UI still appears, the player still
   clicks through it, and it still elevates. What is saved is a browser tab.
   That is not worth what follows.
3. **Supply-chain surface.** This process runs `requireAdministrator`
   (`build.rs`). An elevated process that fetches an executable over the network
   and runs it is a new and serious class of failure — one that the repo has
   already decided how it feels about, in `cbce610` (*ci: pin every action by
   SHA, and fix the one pin that was already wrong*). Doing it safely means
   `WinVerifyTrust` against the Authenticode signature and a check that the
   signing subject is "Nmap Software LLC" — hash pinning is the wrong anchor, it
   goes stale on every Npcap release and the natural fix for a stale pin is to
   stop checking. That is a real amount of `windows-sys` FFI (`Wintrust`), and
   it is FFI whose *failure* mode is "we ran an attacker's binary as
   administrator".
4. **Dependencies.** The crate has no HTTP client and this repo does not add
   dependencies casually — `docs/tech-debt/_HANDOFF.md` records `proptest`,
   `smallvec`, `arrayvec` and `tempfile` all refused with a written argument and
   a measurement, and the `tempfile` refusal went as far as dropping
   `egui_kittest`'s `snapshot` feature to take **14 crates** back out of the
   resolution. `reqwest` would be the largest single dependency in a 413-crate
   lock. A hand-rolled HTTPS GET over the `tokio-rustls` + `webpki-roots` +
   `ring` already present is the cheaper spelling and still costs a redirect
   follower, a `Content-Length` reader and a temp-file writer — new code on the
   one path where being wrong is expensive.
5. **New failure modes with no owner**: a corporate proxy, a partial download, a
   full disk, an antivirus quarantining the fetched exe, npcap.com moving its
   `dist/` path, and the version-pinning question (a pinned URL rots, an
   unpinned one is a moving target).

Reopen this only if the Nmap Project answers a redistribution request in a way
that covers it.

### winget or Chocolatey — rejected on a measurement: there is nothing to call

Measured on the development machine, 2026-08-19, `winget` v1.29.290:

```text
> winget search npcap
Nom        ID                       Version   Correspondance Source
PortFinder packetThrower.PortFinder 4.1.4     Tag: npcap     winget
Win10Pcap  DaiyuuNobori.Win10Pcap   10.2.5002 Tag: winpcap   winget
etl2pcapng Microsoft.etl2pcapng     1.11.0    Tag: winpcap   winget
```

Three unrelated packages matched by tag. **No Npcap.** Corroborated against the
repository rather than the client: `manifests/i/Insecure/` in
`microsoft/winget-pkgs` master now contains only `Nmap`, and the old
`Insecure.Npcap/0.86` installer manifest that search results still surface
**404s**. The open request `microsoft/winget-pkgs#361415` (nmap.npcap 1.87)
carries the labels `Interactive-Only-Installer` and `Blocking-Issue`.

So the option is not "shell out to winget", it is "shell out to a package that
does not exist". And if it ever appears, it inherits the same licence question
one level down, plus a second elevation prompt and a dependency on whether the
player has App Installer at all. Chocolatey is worse on the same axis: not
present by default on any player's machine.

### Do nothing beyond the current error — rejected, and the code says why

This is the honest baseline and it loses on three specific defects, not on
taste. They are listed in the next section because they are the work.

## What happens today, exactly

**Exercised on real hardware, 2026-08-19.** Both x64 `wpcap.dll` copies were
renamed aside on a machine with Npcap 1.10.4 installed, reproducing exactly what
`Wpcap::load` sees where Npcap is absent — its two candidates are `wpcap.dll`
(by name, so System32) and `C:\Windows\System32\Npcap\wpcap.dll`. The console
build was run against that state. This is the first time this path has been
observed rather than reasoned about.

```text
Fatal error: network capture: install Npcap from https://npcap.com/#download and
leave "Restrict Npcap driver's access to Administrators" UNCHECKED. (wpcap.dll
could not be loaded: wpcap.dll: LoadLibraryExW failed;
C:\Windows\System32\Npcap\wpcap.dll: LoadLibraryExW failed)
```

The `Error::Capture` travels `PcapSource::open` → `build_source`
(`src/app/mod.rs`) → the `?` in `Session::run`. The two lanes diverge there, and
only one of them goes through `supervise`: the console arm calls `app::run`
directly and prints `Fatal error: `, while the GUI arm (`main.rs:225`) goes
through `app::supervise`, which prefixes `session error: `, into
`SessionErrorSlot` → `render_status_bar` (`src/ui/statusbar.rs`). They never
stack; a player sees one prefix or the other.

Of the three defects this decision funded, two are fixed (`e16ab73`) and one is
open.

1. **The link is reachable now.** ~~The message arrives as one unbroken
   string…~~ **Fixed, and the original claim here was overstated.** This section
   asserted the line was "upwards of 300" characters. Measured against the live
   output: the whole string is **269** characters and the URL began at character
   **171**. Bad, but not what was written — an estimate repeated as a
   measurement, which is the habit the rest of these documents exist to avoid.
   With the hint moved in front of the diagnostics the URL now begins at
   character **49** (console) or **51** (GUI, whose prefix is two characters
   longer). The candidate paths are still there, behind the sentence that fixes
   the problem, because they are what distinguishes "no Npcap" from "Npcap
   present and the load failed anyway".

   **The address is a link now**, and the second half of this item was wrong
   too. It claimed egui labels "are not selectable by default", so the player
   "must retype" the URL. Measured: `selectable_labels` defaults to `true`
   (egui 0.35 `style.rs:1482`) and the crate never overrides it, so the address
   has always been selectable — the defect was that it had to be picked
   character by character out of a 270-character line, which is tedious rather
   than impossible. Overstated in the same direction as the character count, and
   from the same habit.

   `statusbar::split_help_url` splits the message around its first `https://`
   and the banner renders that piece with `ui.hyperlink_to`. Cost: **zero
   dependencies.** `webbrowser` is already compiled — a non-optional dependency
   of `egui-winit`, which eframe pulls in (`cargo tree -i webbrowser`) — and
   `src/ui/theme.rs:72` was already setting `visuals.hyperlink_color = ACCENT`
   for a hyperlink the crate never drew. (The line is 72, not the 78 this
   document first gave.)
2. **The advice the player had already taken is gone.** ~~`no_usable_device_error`'s
   `AdminOnly` arm ends "…or run this app elevated".~~ **Fixed.** The shipped exe
   is manifested `requireAdministrator` (`build.rs`), so anyone reading that
   message approved a UAC prompt to get there and has nothing left to raise;
   reinstalling with the box unchecked is now the only lever offered.

   Worth recording because it bounds what can ever be tested: that branch is
   close to unreachable in the shipped build *by construction*. `AdminOnly=1`
   restricts non-administrator processes, and this process is always elevated, so
   reaching it needs an elevated process that still enumerates no adapter. A
   `wpcap.dll` rename cannot produce that state, and neither can uninstalling
   Npcap. The fix is a reading of the code, not an observation.
3. **There is still no way back without a relaunch.** `PcapSource::open` runs
   once, from `Session::run`, at startup. A player who reads the message,
   installs Npcap, and returns to the window is looking at a dead session, and
   nothing tells them to restart the app. The README's troubleshooting section
   has the same gap.

   Deliberately not fixed: a Retry button rests on whether a fresh Npcap install
   is visible to an already-running process, and that has not been measured. The
   live run above could not settle it either — it restores a DLL that was present
   when the process started, which is not the same question. Building the button
   on an untested assumption is how `capture-backend-choice.md` got rewritten
   twice.

## What the decision costs

**Dependencies: none.** `ui.hyperlink_to` is already in the eframe that ships.
The re-probe reuses the existing `mpsc::Sender<Command>` the status bar already
sends `Start`/`Stop` on. **Binary size: unmeasured but expected to be noise** —
one egui widget and one string, against a tree that is already 413 crates.

**New failure modes: one, and it is bounded.** A re-probe means
`PcapSource::open` can run more than once in a process. It allocates one
`libloading::Library` and *n* `pcap_t` per attempt and drops them on failure, so
a player who mashes the button leaks nothing; the pre-existing rule that a
`Handle` is closed by `Drop` on its owning thread is what makes that true, and
it is checkable in one file (`sys.rs`'s header says so).

**What is not verified.** Whether a *fresh* Npcap install is visible to an
already-running process without a relaunch is the assumption the re-probe rests
on, and it has not been measured here. It is plausible — the driver service
starts at install time and `wpcap.dll` is resolved by full path on every attempt
— but "plausible" is the word that started the last two rewrites of
`docs/capture-backend-choice.md`. **Measure it before shipping the re-probe**,
and if it turns out a relaunch is needed, the honest implementation is a message
that says "install Npcap, then restart this app" and no button at all. Likewise
untested: whether an Npcap *upgrade* over an in-use driver requires a reboot.
The clean-install case needs none, which is what the README already claims.

## Accepted trade-off

**The player still installs Npcap by hand, and that cost does not go away.** It
is the same trade-off `docs/capture-backend-choice.md` accepted, and this
document does not reduce it by a single click. What it changes is that the app
stops presenting the missing install as a session failure and starts presenting
it as the one remaining setup step — which is all the licence leaves room for,
and, per clause 4, all the licensor wants us to do.

## What would reopen this

- **Written permission from the Nmap Project.** The licence invites the request
  from FOSS authors by name, and asking costs an email. Nothing here should be
  read as a prediction of the answer — it has not been asked.
- **Npcap appearing in `winget` under a publisher-authorised manifest.** That
  would make `winget install` a defensible one-liner, and the measurement above
  is a `winget search npcap` away from being re-run.
- **Npcap becoming redistributable**, by a licence change or by the driver
  moving in-box.
- **The relay acquiring an HTTP client for an unrelated reason**, which would
  delete argument 4 against the runtime download — but not arguments 1, 2, 3
  or 5, which are the ones that actually decide it.

Reopening this because "the player has to install something" is not a new
argument. That is the trade-off, stated above, deliberately.
