# 10 — Type Safety (`type-`)

**Category priority:** MEDIUM
**Rules audited:** 13 · **Files read:** 38 (every `.rs` in `src/`, `examples/`, `build.rs`, plus `Cargo.toml`) · **Findings:** 11 (P0 0 / P1 3 / P2 7 / P3 1)

## Verdict

The crate's *enum* discipline is excellent — `Status`, `StopReason`, `Action`, `Recovery`,
`Command`, `Mode`, `SurfaceError`, `LinkStrip`, `Stage`, `Section`, `HaltSource` are all
real sum types, and the wire boundary parses into them rather than passing strings
around. The hole is entirely on the *newtype* side: this crate has **zero** newtypes over
primitives, so every identifier, counter, currency, handle and generation number in it is
a bare `u32`/`u64`/`u8`/`isize`, and several of them sit adjacent in the same argument
list. The worst offender is `src/actuator/plan.rs`: its three job builders take
`(… , epoch: u64, seed: u64)` back to back, and transposing them silently converts every
click into "the shop changed — dropped planned clicks". The single highest-value fix is
three newtypes — `Epoch(u64)`, `Row(u8)`/`Slot(u8)`, `Hwnd(isize)` — which close all three
P1 holes and cost nothing at runtime.

## Findings

### type-001 — `epoch` and `seed` are two adjacent bare `u64` in every job builder

- **Severity:** P1
- **Rule:** [`type-newtype-ids`](../../.claude/skills/rust-skills/rules/type-newtype-ids.md)
- **Site:** `src/actuator/plan.rs:485` (`confirm_retry_job`), `:498` (`refresh_job`), `:519` (`buy_job`), `:418` (`Job::epoch`); callers `src/app/session/mod.rs:494`, `:577`, `:621`; consumer `src/actuator/mod.rs:375`
- **What:** `pub fn refresh_job(trigger: Trigger, timings: Timings, epoch: u64, seed: u64) -> Job`.
  `epoch` is a shop-generation counter (`SnapshotEpoch::current()`, a small integer);
  `seed` is a millisecond timestamp (`now_ms`). Both are `u64`, adjacent, and every call
  site passes them positionally: `plan::refresh_job(trigger, actuator.timings(), actuator.epoch.current(), now_ms)`.
  `buy_job` separates them only by a `&[u8]`. Transposing them compiles.
- **Why it matters here:** the executor's first act on every job is
  `if job.epoch != epoch.current() { … "the shop changed" }` (`actuator/mod.rs:375`). A
  transposition therefore does not produce a wrong click — it produces **no clicks at
  all**, forever, with a journal line blaming the shop for it. The player sees an armed
  watch, a refreshing counter and nothing happening in game; the log accuses the wrong
  subsystem. Tests would not catch it either: `plan.rs`'s own tests pass literals
  (`refresh_job(Trigger::Refreshed, Timings::default(), 3, 42)`) and
  `session/tests.rs:1096` passes `0, 0`.
- **Fix:** two `#[derive(Debug, Clone, Copy, PartialEq, Eq)]` newtypes:
  `pub struct Epoch(pub u64)` (returned by `SnapshotEpoch::current`, stored in
  `Job::epoch`) and `pub struct Seed(pub u64)` (or take the seed as
  `impl Into<Seed>` from `now_ms`). No call site changes shape; the transposition
  becomes a type error.
- **Effort:** small

### type-002 — window handles travel as bare `isize`, beside an `isize` LPARAM

- **Severity:** P1
- **Rule:** [`type-newtype-ids`](../../.claude/skills/rust-skills/rules/type-newtype-ids.md) (+ [`type-repr-transparent`](../../.claude/skills/rust-skills/rules/type-repr-transparent.md) for the fix)
- **Site:** `src/actuator/win.rs:728` (`fn post(hwnd: isize, msg: u32, wparam: usize, lparam: isize)`), `:76-91` (`InputDriver`: `probe_reachable(hwnd: isize)`, `foreground_window() -> isize`, `request_foreground(hwnd: isize)`, `client_rect(hwnd: isize)`), `:594` (`Target { hwnd: isize }`), `:719` (`pack_point(..) -> isize`); `src/actuator/shield.rs:27` (`static WINDOW: Mutex<Option<isize>>`), `:141` (`fn handle() -> Result<isize, String>`)
- **What:** the same window handle is spelled three ways — `HWND` (`*mut c_void`),
  `isize`, and `as` casts between them (`win.rs:98,102,119,123,613,620,733`;
  `shield.rs:39,135,147,261`). In `post` the handle parameter and the packed-coordinate
  parameter have the *same type*, and `pack_point` — whose result feeds `lparam` — returns
  exactly that type, so `post(lparam, WM_LBUTTONDOWN, MK_LBUTTON as usize, target.hwnd)`
  compiles.
- **Why it matters here:** the `isize` representation is load-bearing and documented
  ("the handle is stored as an integer so the executor's future stays `Send`",
  `win.rs:565`) — so it is not going away, which is precisely why it needs a type. A
  transposed `post` would hand a coordinate pair to `PostMessageW` as a window handle:
  `FALSE` + `ERROR_INVALID_WINDOW_HANDLE`, classified `Recoverable` by
  `post_refusal`, and the watchdog then retries a click that can never land. Every
  `as HWND` cast is also a place a wrong integer is laundered into a pointer with no
  diagnostic.
- **Fix:** one `#[repr(transparent)] #[derive(Clone, Copy, PartialEq, Eq)] pub struct Hwnd(isize)`
  with `fn raw(self) -> HWND { self.0 as HWND }` and `unsafe impl Send`. `repr(transparent)`
  is what keeps the layout guarantee the `as HWND` cast currently relies on implicitly.
  Thread it through `InputDriver`, `Target`, `post`, `shield::{raise, handle, hide}`. While
  there, add `fmt::LowerHex`/`UpperHex` forwarding to the inner value
  ([`type-numeric-fmt`](../../.claude/skills/rust-skills/rules/type-numeric-fmt.md)): handles
  are conventionally logged in hex, and today `{:#x}` on a handle is unavailable the moment
  it is wrapped.
- **Effort:** medium

### type-003 — 1-based slot number and 0-based click row are both `u8`

- **Severity:** P1
- **Rule:** [`type-newtype-ids`](../../.claude/skills/rust-skills/rules/type-newtype-ids.md)
- **Site:** `src/actuator/plan.rs:90` (`row_for_slot(slot: u8) -> Option<u8>`), `:97` (`buy_zone(row: u8, at_bottom: bool)`), `:519` (`buy_job(.., rows: &[u8], ..)`); `src/app/session/mod.rs:600-604` (slot → row), `:614-619` (`row + 1` back to a slot for the journal); `src/domain/control/mod.rs:143` (`BuyTarget::slot`), `src/domain/shop.rs:122` (`effective_slot`), `src/ui/view.rs:41` (`SlotRow::slot`)
- **What:** the display slot (1..=6) and the clickable row (0..=5) differ by one, are both
  `u8`, and the conversion is done by hand in two directions:
  `rows: Vec<u8> = targets.iter().filter_map(|t| plan::row_for_slot(t.slot)).collect()`
  then `format!(">> → buying slot {}", row + 1)`. Nothing prevents
  `plan::buy_job(trigger, timings, epoch, &slots, now_ms)` — passing slot numbers where
  rows are expected — which type-checks perfectly.
- **Why it matters here:** `buy_job` maps rows to screen coordinates via
  `buy_zone(row, at_bottom)`; an off-by-one row clicks the **wrong item's Buy button** and
  then its confirm. That spends the player's gold on an item the filter rejected, and the
  purchase echo for the unexpected id is silently ignored (`on_purchase` finds it absent
  from the checklist), so the pause never clears and the watchdog re-issues the same wrong
  buy twice more before halting. `buy_job` also silently drops `row > 5`
  (`plan.rs:522`), so a slot-numbered `&[1,…,6]` would lose row 6 rather than error.
- **Fix:** `pub struct Slot(pub u8)` (1-based, produced by `effective_slot`) and
  `pub struct Row(pub u8)` (0-based, the only thing `buy_zone`/`buy_job` accept), with
  `Slot::row(self) -> Option<Row>` replacing `row_for_slot` and `Row::slot(self) -> Slot`
  replacing the `row + 1` in the journal line.
- **Effort:** small

### type-004 — catalog ids, gold, crystals and prices are all interchangeable `u32`

- **Severity:** P2
- **Rule:** [`type-newtype-ids`](../../.claude/skills/rust-skills/rules/type-newtype-ids.md)
- **Site:** `src/domain/shop.rs:73` (`ShopItem::id`), `:84` (`price`), `:33-36` (`RefreshMeta { crystal_balance, cost }`), `:152-153` (`PurchaseLimit`); `src/uplink/protocol.rs:31-35` (`PurchaseNotice { item, gold }`); `src/domain/control/mod.rs:113-120` (`Event::Purchase { item: u32, gold: Option<u32> }`), `:251` (`gold_balance`), `:254` (`checklist: Vec<u32>`), `:266` (`bought: Vec<u32>`), `:183-188` (`Progress { refreshes, spent, matches_found }`), `:53-56` (`Limits`); `src/ui/view.rs:23-26`; `src/ui/statusbar.rs:81-82`
- **What:** four unrelated quantities share one type. Two currencies (gold and crystals)
  and one identifier space are all `u32`/`Option<u32>`, and they meet at call sites that
  match them up positionally:
  - `stat_tile(ui, "Skystones", grouped_or_dash(view.crystal_balance)); stat_tile(ui, "Gold", grouped_or_dash(view.gold_balance));`
    — the label↔value pairing is held together by nothing but the author's care.
  - `fn buy_with_gold(item: u32, gold: u32, now_ms: u64)` (`control/tests.rs:67`) — two
    adjacent `u32`, called as `buy_with_gold(102, 100_000, 2)`.
  - `fn meta(crystal_balance: u32, cost: u32)` (`control/tests.rs:35`).
  - `let item = |id: u32, slot: u8, kind, name: &str, price: u32|` (`examples/ui_preview.rs:34`),
    called `item(101, 1, ItemKind::Token, "ticketrare_name", 184_000)`.
- **Why it matters here:** `plan_targets` (`control/mod.rs:521-552`) compares
  `item.price` against `self.gold_balance` while `stop_reason` (`:677`) compares
  `progress.spent` against `limits.max_spend` in crystals — the two ledgers are one type
  apart. Comparing a crystal budget to a gold price would compile and would silently
  veto or authorise every buy. The `checklist`/`bought`/`Event::Purchase::item` id space
  is likewise assignable from any counter.
- **Fix:** `CatalogId(u32)` (produced only by `ShopItem::catalog_id`, consumed by
  `checklist`, `bought`, `BuyTarget::id`, `PurchaseNotice::item`), plus `Gold(u32)` and
  `Crystals(u32)`. Derive `Serialize`/`Deserialize` with `#[serde(transparent)]` so the
  wire shape is unchanged, and give `Gold`/`Crystals` `Display` forwarding to
  `render::grouped` so the status tiles stop pairing labels with values by hand.
- **Effort:** medium

### type-005 — `0` is a sentinel for "absent id" and "absent slot" instead of `Option`

- **Severity:** P2
- **Rule:** [`type-option-nullable`](../../.claude/skills/rust-skills/rules/type-option-nullable.md)
- **Site:** `src/domain/shop.rs:70-76` (`ShopItem { id: u32, slot: u8 }`), `:111` (`catalog_id`), `:122` (`effective_slot`); `src/uplink/protocol.rs:29-31` (`PurchaseNotice::item`); the re-derived test at `src/domain/control/mod.rs:568`
- **What:** `id: u32` uses `0` to mean "the server omitted it", and `slot: u8` does the
  same. `catalog_id()` documents itself as *"The only place the 0 sentinel is interpreted
  — do not re-derive the comparison"* (`shop.rs:108-110`), and the contract is already
  broken: `Controller::on_purchase` re-derives it as `if item != 0 && !self.bought.contains(&item)`
  (`control/mod.rs:568`). `effective_slot` interprets the second sentinel a few lines
  above.
- **Why it matters here:** the sentinel's whole cost is that it must be remembered at
  every use, and the crate has 5 pages of comments and 6 dedicated tests
  (`zero_id_matches_pause_until_new_shop`, `zero_id_echo_never_enters_the_bought_set`,
  `zero_id_slot_disables_dedup`, `buy_target_slot_falls_back_to_position_when_zero`, …)
  keeping that discipline. One forgotten `!= 0` puts a phantom id in `bought` or
  `checklist`, where `catalog_id()` can never match it — a pause that never clears.
- **Fix:** keep the tolerant wire shape but make the domain type honest. Deserialize
  through a helper into `id: Option<NonZeroU32>` / `slot: Option<NonZeroU8>` (a
  `deserialize_with` that maps `0` and absence alike to `None`, exactly what
  `catalog_id`/`effective_slot` compute today), then delete both interpreters and the
  `!= 0` at `control/mod.rs:568`. `PurchaseNotice::item` becomes
  `Option<NonZeroU32>` the same way. Combined with `CatalogId` from type-004,
  `Option<CatalogId>` is the one representation and the sentinel disappears.
- **Effort:** medium

### type-006 — `Deref<Target = [u8]>` on `BudgetedSegment`, which is not a byte container

- **Severity:** P2
- **Rule:** [`type-deref-coercion`](../../.claude/skills/rust-skills/rules/type-deref-coercion.md)
- **Site:** `src/stream.rs:403-409`
- **What:**
  ```rust
  impl Deref for BudgetedSegment {
      type Target = [u8];
      fn deref(&self) -> &Self::Target { self.payload() }
  }
  ```
  `BudgetedSegment` is a four-field struct (`flow`, `seq`, `syn`, `payload`) — a captured
  TCP segment, not a transparent wrapper around bytes. The `Deref` makes every `[u8]`
  method surface on it (`segment.len()`, `segment.is_empty()`, `segment.first()`), reading
  as properties of the segment when they are properties of its payload. It is also
  **dead**: every payload read in the crate goes through the explicit accessor
  (`stream.rs:461,470`, `app/mod.rs:1477`), so nothing depends on the coercion.
- **Why it matters here:** the sibling `impl Deref for BudgetedChunk` (`stream.rs:378`)
  *is* legitimate (owned bytes → borrowed slice, the `String → str` case in the rule's
  "Legitimate Uses"), and it is used (`app/mod.rs:1659`). Having both makes the illegitimate
  one look sanctioned. There is a live trap in the pair, too: `BudgetedChunk::capacity()`
  reports the *lease* size while `.len()` (via `Deref`) reports the current payload length,
  and `HalfStream::absorb` shrinks the latter without the former
  (`payload.bytes.drain(..already)`, `stream.rs:739`). Two same-shaped size reads with
  different meanings, one of them reachable only by coercion, is how a byte-accounting bug
  gets written.
- **Fix:** delete `impl Deref for BudgetedSegment` (nothing compiles differently). Keep
  `BudgetedChunk`'s.
- **Effort:** trivial

### type-007 — the pressure-resync state machine is three `u8` constants, not an enum

- **Severity:** P2
- **Rule:** [`type-enum-states`](../../.claude/skills/rust-skills/rules/type-enum-states.md)
- **Site:** `src/app/mod.rs:63-65` (`RESYNC_ACK`/`RESYNC_PENDING`/`RESYNC_ENQUEUED`), `:70-131` (`PressureResync`)
- **What:** a three-state protocol (`Ack → Pending → Enqueued → Ack`) encoded as bare `u8`
  constants inside an `AtomicU8`, driven by hand-written `compare_exchange` pairs. There is
  no exhaustiveness anywhere: `blocks_segments` is `!= RESYNC_ACK`, `try_enqueue` falls back
  to `load() == RESYNC_ENQUEUED`, and `acknowledge` asserts the previous value with a
  `debug_assert_eq!`. A fourth state added later has no compiler-enforced home.
- **Why it matters here:** `src/watch.rs:23-28` already demonstrates the idiom this file
  wants — `#[repr(u8)] pub enum HaltSource { … }` with explicit discriminants, stored in an
  `AtomicU8`, decoded through one `lowest_in` helper. The two atomics-with-names are
  side by side in the same pipeline and only one of them is typed. This state machine is
  what stands between a byte-pressure event and a lost resync marker (a permanently stalled
  reassembler), so "which state are we in" deserves to be a `match`, not three `!=`.
- **Fix:** `#[repr(u8)] #[derive(Clone, Copy, PartialEq, Eq)] enum Resync { Ack = 0, Pending = 1, Enqueued = 2 }`
  plus `fn from_u8(v: u8) -> Resync` (or `TryFrom`), and express the transitions as
  `compare_exchange(Resync::Ack as u8, Resync::Pending as u8, …)`. The `debug_assert_eq!`
  at `:129` becomes a `match` the compiler checks.
- **Effort:** small

### type-008 — the session's terminal outcome is a `String` plus a separate `bool`

- **Severity:** P2
- **Rule:** [`type-enum-states`](../../.claude/skills/rust-skills/rules/type-enum-states.md)
- **Site:** `src/app/mod.rs:622-636` (`supervise(..) -> (String, bool)`), `src/main.rs:272-291` (`SessionErrorSlot` + `Arc<AtomicBool>`), `src/ui/mod.rs:35` (`pub type SessionErrorSlot = Arc<Mutex<Option<String>>>`), `:147-150` (`session_alive = outcome.is_none()`), `src/main.rs:333-334` (exit code)
- **What:** one terminal state — ended cleanly / failed with a message / panicked — is
  carried as a `(String, bool)` pair, then *split across two containers*: the message into
  an `Arc<Mutex<Option<String>>>` and the flag into an `Arc<AtomicBool>`. The window reads
  only the message (`session_alive = outcome.is_none()`), the exit code reads only the flag.
- **Why it matters here:** the two are written in sequence and not atomically
  (`main.rs:283-290`: `flag.store(true)` then `*slot.lock() = Some(outcome)`), so between
  them the process holds "failed, but the window still says the session is alive" —
  and `Ok(())` from `supervise` also yields a `Some(message)` ("session ended — restart the
  app"), so the banner slot means *both* "clean end" and "failure". Every reader has to
  re-derive which. The rule's Bad example is exactly this shape: a message field plus
  independent booleans for mutually exclusive outcomes.
- **Fix:**
  ```rust
  pub enum SessionOutcome { Ended, Failed(String), Crashed(String) }
  impl SessionOutcome { pub fn failed(&self) -> bool { !matches!(self, Self::Ended) } }
  ```
  `supervise` returns it; the slot becomes `Arc<Mutex<Option<SessionOutcome>>>`; the
  `AtomicBool` and its `Ordering` dance disappear, and the exit code and banner read the
  same value.
- **Effort:** small

### type-009 — `session_loop`'s exit reason is two booleans and an `Option`

- **Severity:** P2
- **Rule:** [`type-enum-states`](../../.claude/skills/rust-skills/rules/type-enum-states.md)
- **Site:** `src/app/session/mod.rs:63-67` (`fatal_failure`, `player_exit`, `uplink_closed`), `:165-186` (all three read after the loop)
- **What:** the loop can end for exactly three reasons, tracked as
  `player_exit: bool`, `uplink_closed: bool` and `fatal_failure: Option<String>` — 8
  representable combinations for 3 real states. The post-loop code reads them in two
  separate decisions: `let teardown = if player_exit { Event::Stop } else { Event::Shutdown }`
  and `uplink_closed && fatal_failure.is_none() && fatal_open` for the 150 ms grace drain.
- **Why it matters here:** the teardown decision is player-visible — it is what makes the
  domain report `PlayerStopped` versus `SessionEnded`, and the crate has three dedicated
  tests pinning that distinction (`session_loop_exit_stops_controller_and_gate`,
  `shutdown_signal_ends_the_loop_and_stops_the_watch`,
  `session_loop_exit_leaves_never_armed_controller_idle`). Encoding it in flags set at four
  different `break` sites means the invariant "exactly one is true" lives in the reader's
  head, and a fifth `break` added later silently defaults to `Event::Shutdown`.
- **Fix:** one local `enum Exit { PlayerStopped, UplinkClosed, Fatal(String) }` assigned at
  each `break` (`let exit = Exit::…; break;`), then a single `match exit` for both the
  grace drain and the teardown event. The `*_open` booleans are unrelated channel-liveness
  flags and should stay as they are.
- **Effort:** small

### type-010 — config invariants are enforced by a private `validate()`, not by construction

- **Severity:** P2
- **Rule:** [`type-newtype-validated`](../../.claude/skills/rust-skills/rules/type-newtype-validated.md)
- **Site:** `src/config.rs:344-424` (`fn validate`, private, called only from `load` at `:331`), `:48` (`server_url: String`), `:44` (`game_port: u16`), `:409-422` (the `[actuator.timings]` walk); `src/actuator/plan.rs:186-189` (`DelayRange { pub min_ms, pub max_ms }`); `src/domain/filter.rs:23` (`kinds: Vec<ItemKind>`)
- **What:** four invariants are checked in one pass over an already-built value instead of
  being carried by types, so every producer that does not come from disk bypasses them:
  1. `server_url: String` must be `wss://`, or `ws://` to loopback. Enforced only in
     `validate`. Downstream code then re-parses the same string ad hoc twice —
     `is_loopback_ws_host` (`config.rs:261`) and `redacted_server_url`
     (`app/mod.rs:262`) — each with its own userinfo/port/IPv6 handling.
  2. `game_port: u16` must be non-zero (`:345`), then flows into
     `format!("tcp and src port {game_port}")` (`pcap.rs:551`).
  3. `DelayRange` must have `min_ms <= max_ms` and `max_ms <= 60_000`. The reversed range is
     *representable*, so the codebase defends against it in four separate places:
     `DelayRange::draw` (`plan.rs:201`), `resolved_band` (`timing_meter.rs:196-197`),
     `pass_estimate` (`editor/mod.rs:538`) and `timing_meter` (`timing_meter.rs:135`),
     all spelling `max_ms.max(min_ms)`. That is the rule's "Bad" example verbatim:
     validation scattered through the consumers.
  4. `Filter::kinds` must not contain `ItemKind::Unknown` (`:375`).
- **Why it matters here:** invariant 4 has already shipped as a bug and was fixed by
  deleting a checkbox rather than by typing — see the comment at `ui/editor/mod.rs:228-233`:
  ticking "?" wrote `kinds = ["unknown"]`, which the *next launch* refused, giving a fatal
  error window and no way out but hand-editing the file the app owns. Any GUI control,
  preset or future in-memory `Config` can reintroduce that class of bug, because
  `Config::validate` is private and only the `load` path runs it — `app::run`/`app::setup`
  accept any `Config` unchecked.
- **Fix:** parse, don't validate:
  - `pub struct ServerUrl(String)` with `ServerUrl::parse(&str) -> Result<Self, Error>`
    holding the scheme+loopback rule, a `Deserialize` impl that goes through it, and the
    `host()`/`redacted()` accessors that today are two free functions.
  - `game_port: NonZeroU16`.
  - make `DelayRange`'s fields private behind
    `DelayRange::new(min_ms, max_ms) -> Result<Self, RangeError>` (ordered and
    `<= MAX_TIMING_MS` by construction) plus `min_ms()`/`max_ms()`; then the four
    `max_ms.max(min_ms)` defences collapse to plain reads.
  - give the filter its own closed kind enum (or `TryFrom<ItemKind>`) so `Unknown` is
    unrepresentable in `Filter::kinds` rather than rejected after the fact.
- **Effort:** medium

### type-011 — coupled fields that permit inconsistent pairs

- **Severity:** P3
- **Rule:** [`type-enum-states`](../../.claude/skills/rust-skills/rules/type-enum-states.md)
- **Site:** `src/domain/control/mod.rs:266-269` (`bought: Vec<u32>` + `bought_fingerprint: Option<Vec<SlotIdentity>>`), `:240` (`started_at: Option<u64>`)
- **What:** two pairs whose consistency is an unwritten invariant.
  `bought` is documented as "the roll `bought` is scoped to" being `bought_fingerprint`, yet
  the two are separate fields: `on_purchase` pushes into `bought` (`:569`) with no
  fingerprint in sight, so "bought ids belonging to no roll" is representable.
  `started_at: Option<u64>` is `Some` exactly when `status != Status::Idle`, but the pair
  is unconstrained — `Status::Watching` with `started_at: None` type-checks and makes
  `duration_elapsed` return `false` forever, silently disabling `max_duration_ms`.
- **Why it matters here:** both invariants currently hold by construction, and the
  controller is the most heavily tested module in the crate — this is debt, not a bug. It
  is worth recording because the fix is small and the failure modes are silent (a re-bought
  slot, a stop limit that never fires).
- **Fix:** group the roll: `struct Roll { identity: Vec<SlotIdentity>, bought: Vec<CatalogId> }`
  and one `bought_roll: Option<Roll>` field — "fresh stock" becomes replacing the `Option`,
  which is already what `evaluate_snapshot:468-471` does in two steps. For `started_at`,
  either fold it into the armed transition or add a `debug_assert!` in `on_tick`.
- **Effort:** small

## Clean areas

- **`type-enum-states` — the domain is exemplary.** `Status`, `StopReason`, `Action`,
  `RefusalReason`, `Recovery`, `Event`, `Proof`, `Mode`, `SubmitError`, `SurfaceError`,
  `Trigger`, `Anchor`, `TimingPreset`, `Input`, `LinkStrip`, `Stage`,
  `HalfOutcome`/`ReassemblyOutcome`, `ForwardStatus`, `AnchorState`, `CaptureEvent`,
  `Outcome`, `Section`, `Tab`, `HaltSource`, `ServerMessage` are all real sum types, several
  carrying exactly the data their state needs (`Status::Stopped(StopReason)`,
  `AnchorState::Buffering { burst, deadline }`, `Recovery::Buy { targets }`). Two of them
  are explicitly relied on for exhaustiveness (`control/mod.rs:577`,
  `plan.rs:264-274`) — do not flatten any of these.
- **`type-no-stringly`** — string input is parsed at the boundary into enums every time:
  `parse_command` (`app/mod.rs:1068`), `#[serde(rename_all)]` + `#[serde(other)]` on
  `ItemKind`/`ServerMessage`, `ActuatorBackend` (with a test proving a typo is rejected),
  and `deny_unknown_fields` on every config section. `Haul`'s `BTreeMap<String, u32>` keyed
  by wire item name is the right call — that key space is open and server-defined — and
  `render::HAUL_HEADLINERS` is the single source for the closed subset the UI names, so
  `count`/`others` never spell a wire id twice.
- **`type-display-vs-debug`** — no `Display` impl in the crate delegates to `Debug`, and no
  `Debug` output reaches the player: the only non-test `{:?}` uses are tracing fields
  (`stage = ?stage`, `since_last_shop_s = ?…`, `?strip`), which is correct for diagnostics.
  The one player-facing `{:?}` (`app/mod.rs:1058`, `">> unknown command: {:?}"`) is quoting a
  `&str` the player typed — it leaks no internal structure and the quotes are the point.
  Player wording lives in `render.rs` as `&'static str` functions (`describe`, `refusal`,
  `kind_label`, `status_summary`) shared by the console and the window rather than in
  `Display` impls; that is a deliberate layering choice, not a swap — leave it.
- **`type-result-fallible`** — `Result` where there is a cause to report (`Config::load`,
  `to_screen`, `Surface::*`, `CaptureStop::stop`, `strip_retired_keys -> Result<Option<_>>`
  distinguishing "failed" from "nothing to do"), `Option` where there is not
  (`parse_segment`, `LinkStrip::for_datalink`, `ShopSnapshot::slot_by_id`,
  `HaltSource::lowest_in`). `PipelineBudget::admit_capture(seg) -> Result<Budgeted, Segment>`
  handing the rejected value back is a good use of `Result` for a refusal.
- **`type-generic-bounds`** — bounds are minimal and `where`-claused where they are long
  (`run_with_connector`, `object_or_none`, `lenient_elements`). Note for the next reader:
  the `S: Surface` bound on `struct SurfaceJobGuard<'a, S: Surface>`
  (`actuator/mod.rs:198`) looks like the rule's "bounds on a struct definition"
  anti-pattern but is **required** — a `Drop` impl must repeat the struct's bounds exactly,
  and `Drop for SurfaceJobGuard` needs `S: Surface` to call `release`. Do not remove it.
- **`type-repr-transparent`** — the four FFI structs in `capture/pcap.rs` are correctly
  `#[repr(C)]` multi-field records with a size canary
  (`the_windows_pcap_pkthdr_is_sixteen_bytes…`), and `HaltSource` is correctly `#[repr(u8)]`
  with explicit bit discriminants. Nothing existing is missing `transparent`.

## Not applicable

- **`type-phantom-marker`** — no type in the crate has an unused type or lifetime
  parameter; there is no typestate or variance to express.
- **`type-never-diverge`** — no function in the crate is non-returning. The closest
  candidates all return: `shield::pump` exits on `WM_QUIT`, `capture_loop` on the stop
  flag, `main::fatal` returns an `ExitCode`, and `std::process::exit` is never called.
- **`type-numeric-fmt`** — no numeric newtype exists yet, so there is nothing to add
  `LowerHex`/`Octal`/`Binary` to. It becomes applicable the moment type-002's
  `Hwnd(isize)` lands (see that fix); `Epoch`/`Slot`/`Row`/`CatalogId` are decimal-only
  counters and need `Display` at most.
- **Missing `#[derive(Debug)]` on internal `pub` types** (`Controller`, `WatchGate`,
  `EventLog`, `ActuatorHandle`, `Reassembler`, `PcapSource`, `ViewState`, …) is
  `api-common-traits`' rule, not `type-display-vs-debug`'s, and this crate is
  `publish = false` — deferred to the `api-` reviewer rather than filed here.
- **`Result<_, String>` error payloads** (`to_screen`, `shield::raise`,
  `open_device`, `Error::Capture(String)`, `SurfaceError::Fatal(String)`) are the `err-`
  category's call, not a `type-` finding.
