# 02 — Error Handling (`err-`)

**Category priority:** CRITICAL
**Rules audited:** 12 · **Files read:** 42 (41 `.rs` + `Cargo.toml`) · **Findings:** 9 (P0 0 / P1 1 / P2 4 / P3 4)

## Verdict

This category is in unusually good shape, and the measurement backs it: `cargo clippy
--bins -- -W clippy::unwrap_used -W clippy::expect_used` reports **zero `unwrap()` and
thirteen `expect()`** across the whole shipped binary, and every one of those thirteen
sits on an invariant rather than on user input, a file, a socket or an FFI return.
`src/error.rs` is a correct `thiserror` enum, `src/config.rs` + `src/config/persist.rs`
are a model of typed, path-carrying, source-preserving config errors, and
`src/actuator/win.rs` converts every Win32 failure into a classified `SurfaceError`
with the OS error interpolated — no panic anywhere on the FFI boundary. `src/capture/pcap.rs`
is likewise panic-free on every `wpcap.dll` return.

The worst offender is **`src/app/session/mod.rs`**: five `.lock().expect("controller mutex
poisoned")` calls in the one module whose death ends the session, in a crate that has
already decided — in five other places, each with a written rationale — that mutex
poisoning must degrade rather than panic. **The single highest-value fix is err-002**:
route those five plus the two in `src/actuator/mod.rs` through one poison-tolerant
`lock` helper. It is a small, mechanical change that removes the only reachable
panic-cascade in the error-handling surface. The next best is err-001, a trivial fix to
a garbled message (`capture: network capture: …`) that is the first thing a player reads
when the pipeline dies.

## Findings

### err-001 — the fatal banner double-prefixes the error kind ("capture: network capture: …")

- **Severity:** P2
- **Rule:** [`err-context-chain`](../../.claude/skills/rust-skills/rules/err-context-chain.md)
- **Site:** `src/app/mod.rs:884` (with `src/error.rs:58-59`); the composed string is asserted verbatim at `src/app/mod.rs:1554`, `:1944`, `:2103`
- **What:** `capture_loop_budgeted` adds the operation name to a string that already opens
  with its own:

  ```rust
  let _ = fatal.blocking_send(format!("capture: {err}"));   // app/mod.rs:884
  #[error("network capture: {0}")] Capture(String),          // error.rs:58-59
  ```

  The result travels through the fatal channel into `Error::Fatal` (`app/mod.rs:439`,
  `#[error("{0}")]`) and into the journal line `>> session aborted — {error}`
  (`session/mod.rs:139`). Its shipped text is
  `capture: network capture: characterization complete`.
- **Why it matters here:** in the windowed build stdout and stderr are inert, so the
  banner and that journal line are the *only* failure channel the player ever sees —
  and the log file is what they are asked to send. The doubled kind is the opening of
  every capture-failure report.
- **Fix:** drop the call-site prefix (`Error::Capture`'s `Display` already names the
  domain): `fatal.blocking_send(err.to_string())`. Update the three assertions to
  `"network capture: …"`.
- **Effort:** trivial

### err-002 — mutex poisoning panics in the session loop and the actuator handle, against the crate's own documented policy

- **Severity:** P1
- **Rule:** [`err-expect-bugs-only`](../../.claude/skills/rust-skills/rules/err-expect-bugs-only.md) (also [`err-result-over-panic`](../../.claude/skills/rust-skills/rules/err-result-over-panic.md))
- **Site:** `src/app/session/mod.rs:200`, `:237`, `:326`, `:357`, `:406` — `.lock().expect("controller mutex poisoned")`; `src/actuator/mod.rs:95`, `:103` — `.lock().expect("actuator timings mutex poisoned")`
- **What:** seven of the crate's fourteen `Mutex::lock` sites panic on poison. The other
  seven do not, and each says why in a comment:
  - `src/actuator/shield.rs:30-34` — *"Poisoning carries no meaning here … a panic elsewhere must not turn every later click into a fatal"*
  - `src/journal.rs:73-80` — *"panicking here after one poisoning would cascade across tasks and freeze the very history the GUI is meant to still show"*
  - `src/ui/mod.rs:37-44` — `lock_ignoring_poison`, *"The view keeps rendering the last state … instead of tearing the window down with a second panic"*
  - `src/main.rs:286-290` — *"panicking here would kill this task silently — no banner, no failed flag — and report a dead session as a clean exit"*
  - `src/stream.rs:147`, `:168`, `:193`, `:248` — `unwrap_or_else(|err| err.into_inner())`
- **Why it matters here:** the rule does permit `expect()` on a bug-indicating invariant,
  and a poisoned mutex is one — so this is not a bare rule violation. It is that the
  crate has already decided *this specific invariant* must degrade, and the seven sites
  that missed the decision are the worst possible ones. The controller guard is held
  across the entire domain state machine (`ctrl.handle(event)`) **and** across `apply()`,
  which formats strings, builds click plans and pushes to a bounded channel — so it has
  real panic surface. It is also taken by the GUI thread through
  `lock_ignoring_poison` (`src/ui/mod.rs:114`, `:139`), which calls `format_item` and
  `haul_tally` under the guard. A panic on the render thread therefore poisons the
  controller, and the *next* `dispatch` in the session loop panics too — turning a
  recoverable frame fault into `supervise` reporting "session crashed" and the whole
  relay stopping, which is exactly the outcome `main.rs` and `journal.rs` document as
  unacceptable. `ActuatorHandle::timings()` is the clearest case of all: it guards a
  `Copy` `Timings` and copies it straight out, so there is nothing a panic could have
  left half-written and poison recovery is unconditionally safe.
- **Fix:** lift one helper into the crate root (or reuse `shield::lock`'s shape) and use
  it at all seven sites:

  ```rust
  pub(crate) fn lock_ignoring_poison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
      mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
  }
  ```

  Then `src/ui/mod.rs:40` and `src/actuator/shield.rs:32` become re-exports of it rather
  than two copies, which is also what keeps the policy from drifting again.
- **Effort:** small

### err-003 — a failed WebSocket send discards its error entirely

- **Severity:** P2
- **Rule:** [`err-context-chain`](../../.claude/skills/rust-skills/rules/err-context-chain.md)
- **Site:** `src/uplink/websocket.rs:190`
- **What:** `Ok(Err(_)) => return Outcome::Disconnected` — the `WsError` from the send is
  dropped on the floor. The read side, twelve lines down, does it right:
  `Some(Err(err)) => { warn!(error = %err, "WebSocket read error"); … }` (`:205-206`).
  The stall branch at `:191-194` logs a message but no error either, and the outage the
  player is told about is the fixed string `"connection interrupted"` (`:123`) — while
  the *connect* failure path does carry the real text (`:132`,
  `inbound.send(UplinkEvent::LinkDown(err.to_string()))`).
- **Why it matters here:** a rejected TLS record, a protocol violation and a socket the
  peer closed all read identically as "server link interrupted" in the log and
  "server link down: connection interrupted" in the journal. This is the relay's only
  outbound path; when it silently reconnect-loops, the log the player sends contains
  nothing to distinguish a Cloudflare rejection from a local network drop.
- **Fix:**

  ```rust
  Ok(Err(err)) => {
      warn!(error = %err, "server send failed");
      return Outcome::Disconnected;
  }
  ```

  and make `Outcome::Disconnected` carry the reason so `:123` reports it instead of the
  literal, matching what `:132` already does.
- **Effort:** trivial

### err-004 — the only reachable `Error::Io` producer surfaces as a bare "i/o: …"

- **Severity:** P2
- **Rule:** [`err-context-chain`](../../.claude/skills/rust-skills/rules/err-context-chain.md) (also [`err-from-impl`](../../.claude/skills/rust-skills/rules/err-from-impl.md))
- **Site:** `src/app/mod.rs:590` (`})?;` on `std::thread::Builder::spawn`), with `src/error.rs:66-67`
- **What:** `Error::Io(#[from] std::io::Error)` is never constructed by hand anywhere in
  the crate (verified by grep). Its single reachable producer is the blanket `?` in
  `spawn_capture_with_budget`, converting the `io::Error` from spawning the capture
  thread. What the player gets is `session error: i/o: <os error>` with no statement of
  what failed. The same crate names the same failure correctly one module over:
  `src/capture/pcap.rs:584`, `Error::Capture(format!("spawning a capture thread: {err}"))`.
- **Why it matters here:** this is the earliest thing that can fail after the config
  loads, and it is the one failure whose cause ("could not start the capture thread")
  the player can act on — an exhausted thread limit or an out-of-memory condition. The
  `#[from]` blanket conversion is what erases it: `?` is the right operator, but not
  with a context-free variant on the other side.
- **Fix:** `.map_err(|source| Error::Capture(format!("starting the capture thread: {source}")))?`.
  With that, `Error::Io` has no producer left and the variant can be deleted — which is
  the honest outcome, since a catch-all `Io` variant in an app-level error enum is
  exactly the context-free funnel `err-context-chain` warns about.
- **Effort:** trivial

### err-005 — `HalfStream`'s byte accounting panics/wraps where the sibling path saturates and logs

- **Severity:** P2
- **Rule:** [`err-result-over-panic`](../../.claude/skills/rust-skills/rules/err-result-over-panic.md)
- **Site:** `src/stream.rs:759` (`self.pending_bytes -= old.capacity()`), `src/stream.rs:785` (`self.pending_bytes -= payload.capacity()`)
- **What:** `PipelineBudget::release` (`src/stream.rs:192-210`) handles exactly this class
  of accounting underflow deliberately — `saturating_sub` plus an `error!` plus a
  `debug_assert!`, with a nine-line comment explaining that a panic from a `Drop` during
  an unwind would abort the process with no `crash.log`. The two `pending_bytes`
  subtractions one layer up, on a counter fed by attacker-influenced payload lengths,
  use the plain operator.
- **Why it matters here:** the invariant *does* hold by construction today (every
  `pending` entry is inserted with its capacity counted at `:771`, and the
  `HalfOutcome::Pressure` path drops the whole `HalfStream` via `Reassembler::clear`), so
  this is not a live bug — it is the one place in the file where the file's own
  defensive policy was not applied. If it ever breaks, the release-build failure mode is
  the nastiest one available: `pending_bytes` wraps to ~2^64, `buffer_future`
  (`:764-770`) then refuses every out-of-order segment, `push` reports `Pressure`, and
  `push_budgeted` clears every flow's anchor on every segment — a permanent, silent
  reassembly stall on the single code path the product exists for, with no panic and no
  log. In debug it panics inside the reassembly task instead, which `SessionWorkers`
  turns into `"reassembly task panicked"`.
- **Fix:** mirror `release`: `saturating_sub`, `debug_assert!`, and an `error!` naming
  `pending_bytes` and the capacity.
- **Effort:** trivial

### err-006 — internal fallible helpers return `String`, which forces one classification to be wrong at the call site

- **Severity:** P3
- **Rule:** [`err-custom-type`](../../.claude/skills/rust-skills/rules/err-custom-type.md)
- **Site:** `src/actuator/plan.rs:130` (`to_screen -> Result<(i32,i32), String>`); also `src/actuator/shield.rs:38` (`raise -> Result<bool, String>`), `src/capture/pcap.rs:728-732` (`open_device -> Result<Handle, String>`), and the three `String`-carrying variants of `crate::Error` (`src/error.rs:12-13`, `:58-59`, `:63-64`)
- **What:** every failure inside these is flattened to a formatted string, so no caller
  can branch on a cause. Honest scoping first: nothing in production ever matches a
  `crate::Error` variant (only tests do) and nothing walks `source()`, so the three
  stringly `Error` variants are currently costing nothing, and `open_device`/`raise`
  return text whose only consumer is a log line or a `SurfaceError` wrapper — both fine.
- **Why it matters here:** `to_screen` is the one that already bites. `run_executor`
  (`src/actuator/mod.rs:318-325`) must map *any* `to_screen` error to
  `SurfaceError::Fatal` → `fail()` → halt the watch. But the function has two failure
  modes with opposite verdicts: `"degenerate client area {w}×{h}"` (`plan.rs:131-136`) is
  the *recoverable* case, which the caller already has to detect a second time itself at
  `mod.rs:305-311` and route to `abort()`; `"window aspect … narrower than 16:9"`
  (`plan.rs:139-143`) is genuinely fatal. The string type is what forced both the
  blanket verdict and the duplicated guard. Everywhere else in this module the
  classification correctly lives at the error's birth site (`post_refusal`,
  `preflight_refusal`, `placement_refusal`).
- **Fix:** give `to_screen` a two-variant error (or return `SurfaceError` directly, since
  `plan` already owns `ClientRect`) so `run_executor` drops its duplicate degenerate-rect
  check and each cause keeps its own verdict. Leave `Error::Config`/`Capture`/`Fatal`
  alone until a caller actually needs to branch — converting them now would be churn.
- **Effort:** small (`to_screen`) · not currently justified (`crate::Error`)

### err-007 — `SurfaceError` and `SubmitError` implement neither `Display` nor `std::error::Error`

- **Severity:** P3
- **Rule:** [`err-thiserror-lib`](../../.claude/skills/rust-skills/rules/err-thiserror-lib.md)
- **Site:** `src/actuator/mod.rs:131-142` (`SurfaceError`), `src/actuator/mod.rs:118-126` (`SubmitError`)
- **What:** both are hand-rolled enums with only `#[derive(Debug, Clone, PartialEq, Eq)]`.
  Every consumer therefore destructures and hand-formats: `src/actuator/mod.rs:288-300`
  and `:351-366` for `SurfaceError`, `src/app/session/mod.rs:518-527` for `SubmitError`.
  `thiserror` is already a direct dependency (`Cargo.toml:52`), used by `src/error.rs`.
- **Why it matters here:** neither can be `?`-propagated into `crate::Error`, logged as
  `error = %err`, or attached as the `#[source]` of anything — so an actuator failure can
  never appear in the crash log with a chain, only as whatever prose the one match arm
  chose to build. `SurfaceError::Fatal(String)` is already a display type in everything
  but the impl: its payload is operator-facing text assembled precisely so a human can
  read it.
- **Fix:** `#[derive(thiserror::Error)]` with `#[error("{0}")]` on both `SurfaceError`
  variants, and one message per `SubmitError` variant. Keep the existing match-based
  journal wording where it differs from the `Display` text (the two `SubmitError` lines
  deliberately give different advice — `src/app/session/mod.rs:503-527`).
- **Effort:** small

### err-008 — `Display` messages inline their own `{source}`, so any chain-printing reporter will double-print

- **Severity:** P3
- **Rule:** [`err-source-chain`](../../.claude/skills/rust-skills/rules/err-source-chain.md)
- **Site:** `src/error.rs:25` and `src/error.rs:37`
- **What:** both variants interpolate the cause into the message *and* expose it as the
  source:

  ```rust
  #[error("could not read {}: {source}", path.display())]
  ConfigRead { path: PathBuf, #[source] source: std::io::Error },
  ```

  The rule's `Good` example does the opposite: `#[error("Failed to read config file '{path}'")]`
  with the cause reachable only through `source()`.
- **Why it matters here:** harmless today, and verifiably so — no production code prints
  `{err:#}` or iterates `source()` (the only reference in the crate is an assertion,
  `src/config/persist.rs:605`). The trap is asymmetric: because there is no
  chain-printing site, a variant added later with `#[source]` and *no* `{source}` in its
  message would lose its cause silently and completely, everywhere. `ConfigParse`,
  `ConfigReparse`, `ConfigSerialize` and `Io` already depend on `{0}` for the same reason,
  so the convention in this file is "Display carries everything" — it just isn't written
  down, and it contradicts what `#[source]` is for.
- **Fix:** pick one and state it in the module doc. Either (a) drop `{source}` from the
  two messages and print the chain at the two report sites — `main::fatal`
  (`src/main.rs:224-234`) and `app::supervise` (`src/app/mod.rs:633`) — with `{err:#}`
  plus a `source()` walk; or (b) drop the two `#[source]` attributes and keep the
  self-contained messages. (a) is better: it is what makes `Error::ConfigRead`'s
  path-carrying design pay off in the crash log too.
- **Effort:** small

### err-009 — 8 of 11 fallible public functions have no `# Errors` section

- **Severity:** P3
- **Rule:** [`err-doc-errors`](../../.claude/skills/rust-skills/rules/err-doc-errors.md)
- **Site:** missing on `app::run` (`src/app/mod.rs:286-288`), `Session::run` (`:302-304`), `PcapSource::open` (`src/capture/pcap.rs:527-537`), `ActuatorHandle::submit` (`src/actuator/mod.rs:80-83`), `plan::to_screen` (`src/actuator/plan.rs:127-130`), `shield::raise` (`src/actuator/shield.rs:36-38`), `PipelineBudget::admit_capture` (`src/stream.rs:126`), `BudgetedChunk::retag_outbound` (`src/stream.rs:342`). Present, and exemplary, on `Config::load` (`src/config.rs:302-325`), `persist::save` (`src/config/persist.rs:34-50`) and `persist::strip_retired_keys` (`src/config/persist.rs:143-155`).
- **What:** the three that have it enumerate every variant, when each fires, and why a
  missing file is deliberately not an error — a genuinely high standard. The other eight
  do not.
- **Why it matters here:** weakened, and honestly so. `publish = false`, four of the eight
  are `pub(crate)`/`pub(super)`, and most describe their failure conditions in prose right
  above the signature (`PcapSource::open`: *"Only zero usable devices is fatal"*;
  `plan::to_screen`: *"A narrower window … unsupported, refused"*). The two that actually
  cost a reader something are `admit_capture` and `retag_outbound`: both have **no doc
  comment at all**, and in both the `Err` payload is not an error but the *value handed
  back* (`Result<BudgetedSegment, Segment>`, `Result<Self, Self>`) — an unusual contract
  that nothing states.
- **Fix:** add `# Errors` to the four `pub` entries; for the two `stream.rs` give-back
  functions, document what the `Err` payload is and who owns it (one line each is enough).
- **Effort:** small

## Clean areas

**`err-no-unwrap-prod` — honoured completely, and measured.**
`cargo clippy --bins -- -W clippy::unwrap_used -W clippy::expect_used` reports **zero
`unwrap()` calls in the entire shipped binary**. There are 13 `expect()` calls, all
listed and accounted for in this report. Every `Option`/`Result` that could carry a real
failure is handled with `?`, `let … else`, `match`, `ok_or_else`, `unwrap_or`,
`unwrap_or_else`, `unwrap_or_default`, or `is_some_and`/`is_none_or`. (A note for the
`lint-` reviewer, since the rule file raises it: `Cargo.toml` has no `[lints.clippy]`
section, so this state is currently maintained by discipline rather than enforced.)

**`err-expect-bugs-only` — every remaining `expect()` is a genuine invariant, none on user
input.** Worth listing so nobody "fixes" them: `src/stream.rs:518`
(`"a burst flow is never empty"` — the flow was just built from a non-empty group),
`:531` (`"every burst slot has one segment"` — slots and queues are built from the same
`Vec`), `:782` (`"peeked above"` — immediately after `first_key_value`);
`src/actuator/mod.rs:210`/`:218` (`"active surface job guard"` — `None` only after
`release_once`, which ends the job); `src/main.rs:120`
(`"install the rustls ring CryptoProvider"` — a process-global install that can only fail
if one is already present, at a point in `main` where none can be, and the rule
explicitly permits main-time initialisation). Test-only `expect()`s (`plan.rs`,
`config.rs`, `persist.rs`, `pcap.rs`, `app/mod.rs` test modules) and
`examples/ui_preview.rs` are exempt by the rule's own text.

**`err-result-over-panic` — the FFI and attacker-facing boundaries are exemplary.**
`src/capture/pcap.rs` checks every `wpcap.dll` return before dereferencing and never
panics: a missing DLL, a missing symbol, a NUL in a device name, a failed
`pcap_findalldevs`/`open_live`/`compile`/`setfilter`, an implausible `caplen`, and a
`Sender::send` on a dead receiver are all `Err` or a clean loop exit.
`plausible_caplen` (`:404-406`) turns the one FFI mistake that would *not* crash into a
detected, named session end. `src/actuator/win.rs` likewise: every `SetWindowPos`,
`FindWindowW`, `GetClientRect`, `ClientToScreen`, `PostMessageW` and `SendInput` failure
becomes a classified `SurfaceError` with `std::io::Error::last_os_error()` read *before*
the next Win32 call. `MessageSurface::target()` (`:575-579`) explicitly documents choosing
`Fatal` over a panic because *"a panic here would kill the actuator task and take the
whole session down"*. `src/stream.rs:726-736` refuses an `as` cast in favour of
`usize::try_from` with an `error!` + `debug_assert!` and a documented
"deliver nothing, do not claim pressure" recovery. `DelayRange::draw`
(`src/actuator/plan.rs:201-214`) guards the `% 0` a `max_ms = u64::MAX` config would
cause, and `slack_from_target` (`src/ui/editor/timing_meter.rs:185-188`) keeps an
`f32::clamp` total against a retuned baseline — both with tests.

**`err-question-mark` — used idiomatically throughout.** `Config::load`, `persist::save`,
`strip_retired_keys`, `Wpcap::load`, `open_device`, `Session::run`, `build_source` all
propagate with `?`; there is no `match`-to-return-`Err` boilerplate anywhere.

**`err-from-impl` — correct.** `#[from]` on `toml::de::Error`, `toml_edit::TomlError`,
`toml_edit::ser::Error` and `std::io::Error` gives `?` for free in
`src/config.rs:330` and `src/config/persist.rs:188`, `:340`, `:358`; the two variants that
need extra context (`ConfigRead`, `ConfigWrite`) correctly use `#[source]` + `map_err`
instead of `#[from]`, which is exactly the split the rule prescribes.

**`err-lowercase-msg` — clean across all nine variants.** `"invalid configuration: {0}"`,
`"configuration parse: {0}"`, `"could not read …"`, `"could not write …"`,
`"config re-parse: {0}"`, `"config serialize: {0}"`, `"network capture: {0}"`, `"{0}"`,
`"i/o: {0}"` — all lowercase, no trailing punctuation, data at the end. The internal
`String` errors follow the same convention (`"pcap_open_live: …"`,
`"could not raise the input shield …"`, `"device name contains a NUL"`). Player-facing
*banner* text is deliberately sentence-cased (`main::fatal`, the `>>` journal lines) —
that is UI copy, not error `Display`, and correctly separated.

**`err-source-chain` — the chain is preserved where it exists.** `ConfigRead`/`ConfigWrite`
carry `#[source] std::io::Error` plus the path, and `src/config/persist.rs:605` has a
regression test asserting `Error::source(&err).is_some()` for `ConfigReparse` — pinning
the very thing a `String` funnel destroyed (see the comment at `:602-605`).

**`err-custom-type` — the domain errors that need to be matchable are.**
`SubmitError::{QueueFull, ExecutorGone}` and `SurfaceError::{Recoverable, Fatal}` exist
specifically so callers can branch, and `src/actuator/mod.rs:110-126` documents the cost
of collapsing them (*"that mistake already cost one full investigation"*).
`StopReason`, `RefusalReason` and `HaltSource` are proper typed outcome enums with
rendering kept in `src/render.rs`. `ForwardStatus`/`HalfOutcome`/`ReassemblyOutcome` are
exhaustively matched by construction with a comment saying why
(`src/stream.rs:574-577`).

**`err-anyhow-app` — correctly judged not to need `anyhow`.** There is no `anyhow`
dependency, and the hand-rolled `crate::Error` genuinely carries the context `anyhow`
would have provided: the two filesystem variants name the path *and* keep the `io::Error`
as a source, `Error::Config` carries a validation message that names the field and both
offending values, and `Error::Capture` names the Win32 call plus the install hint. The
four `err-context-chain` gaps above (err-001, err-003, err-004) are call-site slips, not
evidence that a context-chaining crate is missing.

**Deliberate, documented error *swallowing* that should not be "fixed":**
`src/domain/shop.rs:43-66` (`object_or_none`, `lenient_elements`) drops deserialization
errors on purpose so one malformed side-channel field cannot kill a whole snapshot —
tested at `:176-227`. `src/crash.rs:86-92` swallows every write failure because a panic
hook must not panic. `src/migrate.rs` collects every failure into `Leftovers.warnings`
and reports them once a subscriber exists, with the ordering rationale written out at
`:38-46`. `src/uplink/websocket.rs:216-221` logs and drops undecodable server messages
for forward compatibility.

## Not applicable

- **`err-thiserror-lib` (library-surface half)** — `publish = false`; `crate::Error` is an
  internal report type, so "users can match specific variants" is a weaker goal here than
  the rule assumes. The finding filed under this rule (err-007) is about two types with no
  `Display` at all, which costs the crate regardless of publication.
- **`err-doc-errors` (public-API half)** — same reason; err-009 is filed at P3 and scoped
  to the two entries that have no documentation whatsoever.
- **`err-anyhow-app`** — audited and found correctly declined, not violated (see above).
  No finding.
