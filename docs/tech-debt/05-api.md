# 05 — API Design (`api-`)

**Category priority:** HIGH
**Rules audited:** 17 · **Files read:** 41 (every `.rs` in the crate, plus `Cargo.toml`) · **Findings:** 8 (P0 0 / P1 1 / P2 4 / P3 3)

## Verdict

This is a `publish = false` binary, so the library-surface half of the category
(`api-non-exhaustive`, `api-sealed-trait`, `api-serde-optional`) does not apply and
is not filed. What is left — internal-boundary design — is in better shape than
average: `#[must_use]` is already used deliberately in eleven places, four of them
with custom messages that explain the exact bug they prevent (`ActuatorHandle::submit`,
`migrate::clean_windivert_leftovers`, `install_logging`, `ui::deliver_command`), and
every `Default` impl in the crate is a judged default rather than a derive of
convenience. The two real gaps are both *invariant-carrying*: `Controller::handle`
returns the actions the entire product exists to execute and is not `#[must_use]` —
and `clippy::must_use_candidate` structurally cannot flag it, because it takes
`&mut self`; and `Config::validate` proves the "wss:// or loopback only" rule then
throws the proof away, leaving `server_url` a bare `String` that two separate
hand-rolled authority parsers re-inspect downstream. Worst offender file is
`src/actuator/plan.rs`, whose three job builders each take `epoch: u64, seed: u64`
adjacent — the epoch is the actuator's staleness interlock, and a swap compiles.
Highest-value fix is one line: `#[must_use]` on `Controller::handle`.

## Findings

### api-001 — `Controller::handle` returns the decision the product runs on, and dropping it is silent

- **Severity:** P1
- **Rule:** [`api-must-use`](../../.claude/skills/rust-skills/rules/api-must-use.md)
- **Site:** `src/domain/control/mod.rs:360` (`pub fn handle(&mut self, event: Event) -> Vec<Action>`).
  Live drop of the return value: `examples/ui_preview.rs:159`. Same shape in the
  window's own command carriers: `src/ui/editor/mod.rs:618` (`commit_row -> Vec<Command>`),
  `src/ui/mod.rs:318` (`render_tab_content`), `src/ui/mod.rs:344` (`render_setup_tab`),
  `src/ui/statusbar.rs:18` (`render_status_bar -> Option<Command>`; `Option` is *not*
  a `#[must_use]` type in std, only many of its methods are).
- **What:** `handle` mutates state *before* returning the work: `emit_refresh`
  (`mod.rs:641`) increments `progress.refreshes`, debits `refresh_meta.crystal_balance`,
  and — with recovery on — arms `Expectation::snapshot(now_ms)`; `evaluate_snapshot`
  sets `Status::Paused` and fills `checklist`. The `Vec<Action>` it hands back is the
  only channel by which the refresh or the buy actually happens. Dropping it leaves a
  controller that has paid for a refresh nobody clicked.
- **Why it matters here:** the failure is not a no-op, it is a *misdiagnosis*. With
  the watchdog armed, a dropped `Action::Refresh` runs the whole recovery ladder
  (confirm re-click → re-issue → `StopReason::Unresponsive`) and tells the player
  "no response from the game — see the journal" for a click the app never sent.
  `examples/ui_preview.rs:159` already does exactly this (deliberately — the preview
  has no actuator), which is the point: nothing in the type distinguishes that
  deliberate drop from an accidental one in a new call path. And this is the one
  member of the family clippy cannot help with: `clippy::must_use_candidate` reported
  41 candidates across the lib (see api-006) and `handle` is not among them, because
  the lint only fires on `&self`/by-value functions.
- **Fix:** `#[must_use = "these actions are the decision — dropping them loses the refresh or the buy"]`
  on `Controller::handle`, and the same (plain) on the four UI command carriers above.
  `examples/ui_preview.rs:159` becomes `let _ = ctrl.handle(event);`, which documents
  the intent it currently only implies.
- **Effort:** trivial

### api-002 — `server_url` leaves `validate()` as a bare `String`, and two hand-rolled parsers re-inspect it downstream

- **Severity:** P2
- **Rule:** [`api-parse-dont-validate`](../../.claude/skills/rust-skills/rules/api-parse-dont-validate.md)
- **Site:** `src/config.rs:48` (`pub server_url: String`), validated at
  `src/config.rs:354-372` with the loopback test at `src/config.rs:261`
  (`is_loopback_ws_host`); re-parsed independently at `src/app/mod.rs:262`
  (`redacted_server_url`); consumed as a plain `String` at `src/app/mod.rs:352` →
  `src/uplink/websocket.rs:74`.
- **What:** `validate` proves a security property — "this URL is `wss://`, or `ws://`
  to loopback, so the captured game stream never leaves the machine in cleartext" —
  and then discards the proof. `Config` keeps a public `String` field, so any
  `Config` that did not come through `Config::load` (a struct literal, a mutated
  `Config::default()`, a future GUI "server URL" field) reaches `uplink::run`
  unchecked. Meanwhile the same authority-splitting logic is written twice:
  `is_loopback_ws_host` strips userinfo after the last `@` to defeat
  `ws://127.0.0.1@evil.com`, and `redacted_server_url` strips userinfo after the
  last `@` to keep credentials out of the log — near-identical code, no shared
  parse, and only the first one has the userinfo test suite.
- **Why it matters here:** the two duplicated parsers are the concrete cost. The
  `ws://127.0.0.1@evil.com` bypass was found and fixed once (tests at
  `config.rs:942` and `1268`); `redacted_server_url` has its own separate test
  (`app/mod.rs:2223`) and its own separate implementation, so the next parsing
  subtlety has to be found and fixed twice. A single parsed type would also make
  the cleartext rule un-forgettable rather than dependent on `load` being the only
  constructor.
- **Fix:** a `ServerUrl` newtype in `config.rs`:
  ```rust
  pub struct ServerUrl { raw: String, host: String, tls: bool }
  impl ServerUrl {
      pub fn parse(raw: &str) -> Result<Self, Error>;   // scheme + authority + loopback rule
      pub fn as_str(&self) -> &str;                      // what connect_async dials
      pub fn redacted(&self) -> String;                  // "{scheme}://{host}" for logs
  }
  ```
  with `#[serde(try_from = "String")]` so the `[server_url]` key parses through it
  (`serde-try-from-validate`, sibling category). `Config.server_url: ServerUrl`,
  `uplink::run(url: ServerUrl, …)`, and `app::redacted_server_url` deletes.
- **Effort:** small

### api-003 — the job builders take `epoch: u64, seed: u64` adjacent; the epoch is the actuator's staleness interlock

- **Severity:** P2
- **Rule:** [`api-newtype-safety`](../../.claude/skills/rust-skills/rules/api-newtype-safety.md)
- **Site:** `src/actuator/plan.rs:485` (`confirm_retry_job(zone, timings, epoch: u64, seed: u64)`),
  `:498` (`refresh_job(trigger, timings, epoch: u64, seed: u64)`), `:519`
  (`buy_job(trigger, timings, epoch: u64, rows, seed: u64)`), plus `Job.epoch: u64`
  (`:418`) and `Jitter::new(seed: u64)` (`:428`). Call sites pass them adjacent from
  two unrelated sources: `src/app/session/mod.rs:494-499`, `:577`, `:621-627`
  (`actuator.epoch.current(), now_ms`). Second site, same defect shape:
  `src/uplink/websocket.rs:74-80` / `src/app/mod.rs:355-356`, where
  `run(url, out, in, initial_backoff: Duration, max_backoff: Duration)` takes two
  adjacent `Duration`s fed by `config.reconnect_initial(), config.reconnect_max()`.
- **What:** two same-typed parameters with unrelated meanings, side by side. The
  epoch is the shop generation the plan was built against; the seed is the jitter
  stream (`now_ms` in practice). Swapping them compiles.
- **Why it matters here:** the epoch is a safety interlock, not a label —
  `drop_reason` (`src/actuator/mod.rs:374`) refuses any job whose `epoch !=
  epoch.current()`, so a swapped pair either silently kills every job with
  `"the shop changed — dropped planned clicks"` (which reads as an actuator bug and
  cost one full investigation the last time a label was wrong, per the
  `SubmitError` doc comment), or — if the substituted value happens to match the
  live counter — clicks coordinates planned for a shop that no longer exists. Note
  the crate already newtypes the shared *counter* (`SnapshotEpoch`, `actuator/mod.rs:28`)
  while its *value* travels bare through `current() -> u64`. On the uplink side a
  swap is quieter and therefore worse: `Backoff::new` normalizes with
  `max = max.max(initial)` (`websocket.rs:36-37`), so swapped arguments silently
  produce a constant 30 s retry instead of the 1 s → 30 s ramp, with no error
  anywhere.
- **Fix:** `#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub struct Epoch(pub u64);`
  returned by `SnapshotEpoch::current()`, stored in `Job.epoch`, compared in
  `drop_reason`; `pub struct Seed(pub u64)` taken by `Jitter::new` and the three
  builders. Both zero-cost. For the uplink, take one `ReconnectPolicy { initial, max }`
  (built by `Config`) instead of two `Duration`s — the fields are then named at the
  call site.
- **Effort:** small

### api-004 — `Surface`'s acquire→click precondition is enforced three times at runtime instead of once in the type

- **Severity:** P2
- **Rule:** [`api-typestate`](../../.claude/skills/rust-skills/rules/api-typestate.md)
- **Site:** trait and prose precondition at `src/actuator/mod.rs:150-170`; the
  runtime enforcement duplicated per backend at `src/actuator/win.rs:165-169`
  (`WinSurface::target`) and `src/actuator/win.rs:575-579` (`MessageSurface::target`)
  — same `Option<Target>` field (`win.rs:144`, `:566`), same
  `SurfaceError::Fatal("input attempted without an acquired game window")` string;
  a third guard on the same invariant at `src/actuator/mod.rs:213` and `:221`
  (`.expect("active surface job guard")`).
- **What:** this is the `api-typestate` Bad example almost verbatim — a struct with
  `state: Option<Handle>` whose methods begin with a runtime check that returns
  `Err(NotConnected)`. `click` and `scroll` are documented as "preconditioned on a
  successful `acquire` since the last `release`", and each of the two Windows
  backends re-implements that check by hand.
- **Why it matters here:** the executor *already* holds the acquired state. `run_executor`
  takes the `ClientRect` out of `acquire` (`mod.rs:286`) and carries it through the
  whole job for `to_screen`, and `SurfaceJobGuard` exists precisely to scope
  "acquired" to a value with a `Drop`. So the `Option<Target>` inside each backend is
  a *second, unsynchronized copy* of state the caller owns, kept only so the two
  fatals can exist. Three defensive checks for one invariant is three places to get
  the next backend wrong.
- **Fix:** no GATs needed — give the trait an associated handle that `acquire`
  produces and the input methods consume:
  ```rust
  pub trait Surface {
      type Window;                                    // opaque, backend-owned
      fn acquire(&mut self) -> Result<(Self::Window, plan::ClientRect), SurfaceError>;
      fn click(&mut self, window: &Self::Window, at: (i32, i32), press_ms: u64) -> Result<(), SurfaceError>;
      fn scroll(&mut self, window: &Self::Window, at: (i32, i32), notches: i32) -> Result<(), SurfaceError>;
      fn release(&mut self, window: Self::Window) {}
  }
  ```
  `SurfaceJobGuard` holds the `Window`; both `Option<Target>` fields, both
  `target()` methods, both fatal strings and the guard's `expect` all delete, and
  "input without acquire" stops being expressible. Keep the fail-closed rule for
  everything the *world* can break (window died, moved, refused) — that is what
  `SurfaceError` is for and it stays.
- **Effort:** medium

### api-005 — the "filter is restricted enough to arm" invariant is re-derived at five sites

- **Severity:** P2
- **Rule:** [`api-parse-dont-validate`](../../.claude/skills/rust-skills/rules/api-parse-dont-validate.md)
- **Site:** predicate at `src/domain/filter.rs:97` (`Filter::is_unrestricted`), called at
  `src/app/mod.rs:292` (console build refuses to boot), `src/domain/control/mod.rs:373`
  (`FilterChanged` refuses), `src/domain/control/mod.rs:402` (`on_start` refuses),
  `src/ui/editor/mod.rs:629` (Apply stays dark), `src/render.rs:70` (status hint).
- **What:** `Filter` is both the TOML parse target and the live-editable draft, and
  its `Default` matches every item — a state the relay treats as a configuration
  error. Every path that arms the loop, or explains why it will not, has to remember
  to ask. That is the rule's "validation scattered throughout the codebase, did
  someone check already?" shape.
- **Why it matters here:** the invariant guards crystal spend, and a new arming path
  that forgets the check arms a hunt-everything loop that buys the first thing it
  sees. Judged honestly, the current design is *deliberate* — `control/tests.rs:112`
  states "the invariant lives here, not in the callers", and the domain's
  `Action::Refused(RefusalReason::UnrestrictedFilter)` is what lets the console and
  the window render one shared wording. So the fix must not move enforcement out to
  the producers; it should give the domain a type that cannot be built wrong.
- **Fix:** `Filter::restricted(self) -> Result<HuntFilter, Filter>` plus
  `struct HuntFilter(Filter)` with `Deref<Target = Filter>`. `Controller` keeps the
  editable `Filter` for the view, but `Controller::new` and the arming path store a
  `HuntFilter`; `Event::FilterChanged(Filter)` stays as-is so the domain keeps
  emitting the single refusal, and it becomes the *only* place that calls
  `restricted()`. `render.rs:70` and `editor/mod.rs:629` keep using
  `is_unrestricted()` as the UX affordance they are, and `app::run`'s duplicate
  check (`app/mod.rs:292`) can then be deleted in favour of the domain's refusal.
- **Effort:** medium

### api-006 — 41 `must_use_candidate` warnings; the ones where a dropped value is a lost click deserve the attribute

- **Severity:** P3
- **Rule:** [`api-must-use`](../../.claude/skills/rust-skills/rules/api-must-use.md)
- **Site:** measured, not asserted —
  `cargo clippy --lib -- -A clippy::all -W clippy::must_use_candidate` reports 41,
  including `plan.rs:485/498/519` (the three job builders), `plan.rs:90` (`row_for_slot`),
  `plan.rs:97` (`buy_zone`), `app/mod.rs:218` (`setup`), `capture/ip.rs:20`
  (`parse_segment`), `config.rs:426/430` (`reconnect_initial`/`reconnect_max`),
  thirteen `Controller` accessors (`control/mod.rs:170`–`:356`), `filter.rs:55/97`,
  `shop.rs:26/104/111/122`, `journal.rs:44/94`, `stream.rs:545`, `watch.rs:53/63`.
- **What:** the crate applies `#[must_use]` deliberately in eleven places but not
  systematically, so the pure constructors that *build the thing that gets clicked*
  are unmarked while smaller helpers are marked.
- **Why it matters here:** blanket-annotating 41 items is noise. Three of them are
  not: `plan::refresh_job`, `plan::buy_job` and `plan::confirm_retry_job` allocate a
  `Job` and have no other effect, so a dropped one is a click that was planned and
  never submitted — exactly the failure `ActuatorHandle::submit`'s own
  `#[must_use = "a rejected job means a lost click — journal the drop"]` exists to
  catch, one call earlier in the same chain. `plan::to_screen` (`plan.rs:130`, not
  flagged because it returns a `Result`, already `must_use`) closes the same chain.
- **Fix:** annotate the three job builders (with the `submit` wording), then decide
  the rest once by enabling `must_use_candidate` in a `[lints.clippy]` table — that
  table's design belongs to the `lint-` category, not here.
- **Effort:** small

### api-007 — `PipelineBudget::with_test_limits` takes four positional `usize`s over a struct that already exists

- **Severity:** P3
- **Rule:** [`api-builder-pattern`](../../.claude/skills/rust-skills/rules/api-builder-pattern.md)
- **Site:** `src/stream.rs:112-124`; call sites `src/app/mod.rs:1328`, `:1361`, `:1407`
  (e.g. `PipelineBudget::with_test_limits(128, 128, 128, 64)`). `stream.rs`'s own
  helper wraps it the same way (`test_budget(global, capture, reassembly, outbound)`,
  `stream.rs:873`).
- **What:** the rule's Bad example — `Client::new(url, 30, true, None, …)`, "which is
  which?" — with four `usize`s in a fixed order. The named struct it should take
  already exists two screens up: `BudgetLimits { global, capture, reassembly, outbound }`
  (`stream.rs:45`, `Clone + Copy`), and the non-test constructor `with_limits`
  already takes it.
- **Why it matters here:** these are the seams that pin the byte-budget guarantees
  (`stalled_outbound_never_exceeds_pipeline_budget`,
  `steady_pending_pressure_rearms_the_initial_anchor_window`). Two transposed
  arguments do not fail the test — they silently change which stage the test is
  proving, so the guarantee quietly stops being covered. Test-only, hence P3.
- **Fix:** `pub(crate) fn with_test_limits(limits: BudgetLimits) -> Self` (or make
  `with_limits` itself `pub(crate)` and delete the wrapper); call sites become
  `BudgetLimits { global: 128, capture: 128, reassembly: 128, outbound: 64 }`.
- **Effort:** trivial

### api-008 — `config::persist::Section` derives nothing

- **Severity:** P3
- **Rule:** [`api-common-traits`](../../.claude/skills/rust-skills/rules/api-common-traits.md)
- **Site:** `src/config/persist.rs:25` (`pub enum Section { Filter(Filter), Limits(Limits), Timings(Timings) }`)
- **What:** the one type in the crate that carries player data across a module
  boundary with no derives at all. Every payload it wraps has
  `Debug + Clone + PartialEq`; the wrapper has none. Its sibling on the same path,
  `app::Command`, has `Debug, Clone, PartialEq` (`app/mod.rs:149`).
- **Why it matters here:** `save` is best-effort and its failure is journaled at
  `ui/mod.rs:231-236` as `"config.toml not saved: {err}"` — with no way to name
  which section was being written, because the `&[Section]` cannot be formatted. The
  tests are pushed into `matches!(sections[0], Section::Limits(_))`
  (`ui/mod.rs:433`) where `assert_eq!` would say more on failure.
- **Fix:** `#[derive(Debug, Clone, PartialEq)]` on `Section`, and include the
  section names in the journal line.
- **Effort:** trivial

## Clean areas

- **`api-default-impl` — clean, and unusually well judged.** Every `Default` in the
  crate is a decision, not a derive of convenience: `ReconnectConfig` is hand-written
  for its non-zero 1 s/30 s (`config.rs:207`, exactly the rule's "implement by hand
  for a non-zero default"); `ActuatorBackend::default = Message` is the
  live-validated backend that leaves the player their mouse, documented and pinned
  by a test (`config.rs:723`); `Timings::default`/`DelayRange::default` are the
  calibrated baseline with no extra wait; `ItemKind::default = Unknown` pairs with
  `#[serde(other)]` for wire tolerance and `Config::validate` then refuses it in a
  config file. `Config::default()` is a bootable-but-unarmable config *on purpose*
  (the GUI must open so the player can define a hunt) and `Config::default().validate()`
  is asserted `is_ok()` at `config.rs:816`. Types with no sensible default correctly
  have none: `Controller`, `EditorState`, `Jitter`, `ShopApp`.
- **`api-must-use` — the existing eleven are the right eleven**, and four carry
  custom messages that name the bug: `ActuatorHandle::submit` (`actuator/mod.rs:82`),
  `migrate::clean_windivert_leftovers` (`migrate.rs:83`), `install_logging`
  (`main.rs:66`), plus `ui::deliver_command` (`ui/mod.rs:250`) and
  `Target::to_client` (`win.rs:603`) where a dropped result is an input at the wrong
  coordinates. This is a partially-adopted good habit, not an absent one.
- **`api-impl-asref` — correct everywhere it appears.** `Config::load`,
  `persist::save` and `persist::strip_retired_keys` all take `impl AsRef<Path>`
  (borrow-only, no allocation), which is exactly the rule's guidance for a path a
  function only reads.
- **`api-impl-into` — used, and justified in prose.** `ui::shop::styled` takes
  `impl Into<String>` with a comment (`shop.rs:87-94`) explaining that it mirrors
  `RichText::new` so each caller pays at most one copy — that is the rule's
  reasoning, written down at the site. Same at `theme::emphasis`.
- **`api-from-not-into` — vacuously clean and worth stating:** the crate contains
  no `impl Into<…> for …` anywhere (grep: zero hits), so the anti-pattern the rule
  exists to prevent is absent. Error conversions go through `#[from]` on
  `thiserror` variants (`error.rs`), which is the `From` direction.
- **`api-newtype-safety` — already applied where it counts most.** `FlowKey`
  (`capture/mod.rs:57`) stores its two endpoints *by role* rather than by direction
  with a comment explaining why; `SnapshotEpoch`, `PipelineBudget`, `PayloadLease`,
  `WatchGate`, `SnapshotEpoch` and `PressureResync` are all newtypes over shared
  primitives; `DesignPoint`/`ClientRect` keep design space and screen space from
  mixing, which is the rule's Miles/Kilometers case handled correctly. api-003 is
  the remaining gap, not a systemic absence.
- **`api-typestate` — one instance already built correctly:** `SurfaceJobGuard`
  (`actuator/mod.rs:198`) makes "released exactly once, including on unwind" a
  property of a scope rather than a discipline, with three tests pinning it
  (`mod.rs:518-555`). api-004 asks to finish the job, not to start it.
- **`api-builder-pattern` — genuinely not needed.** No type in the crate has a
  pile of optional construction parameters; the many-argument *functions*
  (`session_loop`, `capture_loop_budgeted`, `spawn_capture_with_budget`,
  `run_executor`) take heterogeneous, individually-typed handles and channels, and
  the one that crosses clippy's threshold carries an
  `#[expect(clippy::too_many_arguments, reason = …)]` that argues the case
  (`app/session/mod.rs:34-38`). api-007 is the single exception.

## Not applicable

- **`api-non-exhaustive`** — `publish = false`. Adding a variant or a field breaks
  no downstream crate; it breaks this repo's own build, immediately and visibly
  (e.g. `SessionHandles`'s struct literal in `examples/ui_preview.rs:128`). Marking
  the crate's enums `#[non_exhaustive]` would only force wildcard arms inside the
  crate, which is a net loss: `pat-exhaustive-enum` matching is what makes adding a
  `StopReason` or an `Action` a compile error at every render site today.
- **`api-sealed-trait`** — the three traits (`Surface`, `PacketSource`,
  `CaptureStop`, plus the private `InputDriver`) are already unreachable from
  outside: `PacketSource`/`CaptureStop` are `pub(crate)`-facing and `Surface`'s only
  external implementors would be test fakes, which is the one thing sealing would
  break. Nothing to seal against.
- **`api-serde-optional`** — serde is load-bearing for the whole product (TOML
  config, JSON wire protocol), which is the rule's own "✅ Required: domain heavily
  uses serde" case. A feature flag would gate the binary's reason to exist.
- **`api-common-traits`** — applies weakly: no type here is consumed by third-party
  code, and two omissions are deliberate and documented (`BudgetedChunk`'s `Debug`
  is `#[cfg(test)]`-only so captured payloads cannot reach a log; `Target`,
  `ViewState` and `SlotRow` are frame-local view structs). api-008 is the only case
  where the omission costs something today.
- **`api-impl-fromiterator`** — no general-purpose collection type exists.
  `InitialBurst` (`stream.rs:444`) asserts its byte/segment caps were checked
  *before* `push`, so a `FromIterator` that fed it blindly would trip that assert;
  `Haul` (`control/mod.rs:196`) is a name-keyed tally, not a container of items.
  Implementing either would invite misuse.
- **`api-operator-overload`** — the crate overloads no arithmetic, indexing or set
  operator. The two `Deref` impls (`stream.rs:378`, `:403`) are transparent
  byte-payload wrappers, i.e. the smart-pointer case; whether they should coexist
  with the inherent `as_slice()` is a `type-deref-coercion` question, not an
  operator-semantics one.
- **`api-extension-trait`** — no orphan-rule problem anywhere: every trait in the
  crate is implemented on a local type, so there is nothing to extend a foreign
  type with.
