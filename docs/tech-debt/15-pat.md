# 15 — Pattern Matching (`pat-`)

**Category priority:** MEDIUM
**Rules audited:** 5 · **Files read:** 41 · **Findings:** 8 (P0 0 / P1 0 / P2 3 / P3 5)

## Verdict

This category is in unusually good shape. The crate already treats the compiler
as a checklist almost everywhere: `render.rs`, `ui/theme.rs::status_color`,
`app/session/mod.rs::apply`/`submit_recovery`, `actuator/plan.rs`, `stream.rs`
and `ui/editor/mod.rs::mark_applied` all enumerate every variant of every
crate-owned enum, several with comments saying the exhaustiveness is deliberate
(`stream.rs:575`, `plan.rs:262`). `matches!` is used 74 times and clippy's
`match_like_matches_macro` finds nothing; `let ... else` is used 28 times and
`manual_let_else` finds exactly one site, in test code. There are **no range
patterns in any match arm in the whole crate**, so `pat-at-bindings` has nothing
to apply to.

What is left is a small, precise set of catch-alls over *crate-owned* enums:
`clippy::wildcard_enum_match_arm` reports exactly five, and one more exists that
the lint structurally cannot see. The worst offender file is
`src/domain/control/watchdog.rs`, and the single highest-value fix is
**pat-001**: the recovery ladder matches on the tuple `(Proof, u8)`, so the `_`
arm that the numeric dimension *requires* also silently swallows any new `Proof`
variant — and swallows it into `self.halt(StopReason::Unresponsive)`, i.e. a new
expected proof type would halt the hunt on its first tick and tell the player the
game stopped answering. Two lines fix it.

## Findings

### pat-001 — the recovery ladder's required `_` arm also hides new `Proof` variants, and halts on them

- **Severity:** P2
- **Rule:** [`pat-exhaustive-enum`](../../.claude/skills/rust-skills/rules/pat-exhaustive-enum.md)
- **Site:** `src/domain/control/watchdog.rs:88` (the `match`), `:123` (the `_` arm)
- **What:** the ladder dispatches on a tuple whose second element is a `u8`:

  ```rust
  match (expectation.proof, expectation.attempt) {
      (Proof::Snapshot, 0) => { /* blind confirm re-click */ }
      (Proof::Snapshot, 1) => { /* paid re-issue */ }
      (Proof::Purchase, 0) => { /* blind confirm re-click */ }
      (Proof::Purchase, 1) => { /* re-issue the buys */ }
      _ => self.halt(StopReason::Unresponsive),
  }
  ```

  The `_` is unavoidable *for the `attempt` dimension* (rung ≥ 2 is the halt),
  but it covers the enum dimension too. A third `Proof` variant would match `_`
  at rung 0 and halt immediately.
- **Why it matters here:** `Proof` is the crate's own private enum
  (`watchdog.rs:16`) and it is exactly the kind of thing that grows — a new
  awaited wire proof (a shop-close echo, a scroll acknowledgement) is a plausible
  next feature. The failure would not be a no-op: the very first tick after the
  new expectation is armed lands in `_`, the session stops with
  `StopReason::Unresponsive`, and the player reads "no response from the game —
  see the journal" (`render.rs:105`) about a game that answered fine. This is the
  one site in the crate where a hidden variant produces a *wrong halt* rather
  than a silent skip, and `clippy::wildcard_enum_match_arm` cannot warn about it
  because the scrutinee is a tuple, not an enum — so nothing else will catch it.
- **Fix:** name the enum dimension in the fallback arm, keeping the numeric
  wildcard:

  ```rust
  (Proof::Snapshot | Proof::Purchase, _) => self.halt(StopReason::Unresponsive),
  ```

  Adding a `Proof` variant then fails to compile here until it is placed in the
  ladder. (Nesting `match expectation.proof { … }` around two inner
  `match expectation.attempt` blocks works too but doubles the arms for no gain.)
- **Effort:** trivial

### pat-002 — `persisted_sections` drops unknown `Command`s into `_`, so a new `Set*` would apply live and never be saved

- **Severity:** P2
- **Rule:** [`pat-exhaustive-enum`](../../.claude/skills/rust-skills/rules/pat-exhaustive-enum.md)
- **Site:** `src/ui/mod.rs:310` (clippy-confirmed: `wildcard match will also match any future added variants: help: try: Command::Start | Command::Stop | Command::Toggle`)
- **What:**

  ```rust
  Command::SetFilter(filter) => Some(config::persist::Section::Filter(filter.clone())),
  Command::SetLimits(limits) => Some(config::persist::Section::Limits(limits.clone())),
  Command::SetTimings(timings) => Some(config::persist::Section::Timings(*timings)),
  _ => None,
  ```

  `Command` is crate-owned (`src/app/mod.rs:150`).
- **Why it matters here:** this function is the *only* bridge from a delivered
  Apply to `config.toml`. A fourth `Set*` variant would retune the live session
  and then silently vanish on the next launch — the exact class of "the setting I
  chose reverted" bug the surrounding code fights hard against (see the
  `skip_serializing_if` reasoning in `plan.rs:220-227` and the
  `only_the_all_zero_range_is_inert` test). The `Section` enum would also have to
  grow, but nothing links the two: `write_sections` (`config/persist.rs:343`) is
  exhaustive over `Section`, so the compiler would demand the new section be
  *written* while never demanding it be *collected*.
  The clinching argument that this is a slip and not a policy: the sibling
  function `EditorState::mark_applied` (`src/ui/editor/mod.rs:78-83`) does the
  same three-way dispatch over the same enum and spells the non-`Set*` arm out —
  `Command::Start | Command::Stop | Command::Toggle => {}` — with a comment
  saying why.
- **Fix:** replace `_ => None` with `Command::Start | Command::Stop | Command::Toggle => None`.
- **Effort:** trivial

### pat-003 — `Controller::on_tick`'s `_` arm hides a new `Status` behind "do nothing on every tick"

- **Severity:** P2
- **Rule:** [`pat-exhaustive-enum`](../../.claude/skills/rust-skills/rules/pat-exhaustive-enum.md)
- **Site:** `src/domain/control/mod.rs:625` (clippy-confirmed: `help: try: Status::Idle | Status::Stopped(_)`)
- **What:** `on_tick` has three guarded `Status::Watching`/`Status::Paused` arms
  and then `_ => Vec::new()`. Guards mean *some* fallback arm is required, but it
  does not have to be a wildcard.
- **Why it matters here:** the tick is the controller's only time-driven
  check-point — the duration limit, the mid-session limit retune, and the whole
  recovery watchdog run from here (see the method's own doc comment). A new
  `Status` variant (the natural candidate being an "arming/awaiting-first-shop"
  state) would land in `_` and get *no* limit enforcement and *no* watchdog for
  as long as it lasted: a session parked there would never time out and never
  escalate a missed refresh. Silent, and invisible in the logs, since the
  heartbeat only prints the status word.
- **Fix:** `Status::Idle | Status::Stopped(_) => Vec::new()`.
- **Effort:** trivial

### pat-004 — two remaining crate-owned wildcards: the UI preview's toggle and a test-only call filter

- **Severity:** P3
- **Rule:** [`pat-exhaustive-enum`](../../.claude/skills/rust-skills/rules/pat-exhaustive-enum.md)
- **Site:** `examples/ui_preview.rs:150`, `src/actuator/win.rs:915` (both clippy-confirmed)
- **What:**
  - `examples/ui_preview.rs:148-151` resolves `Command::Toggle` with
    `Status::Watching | Status::Paused => Event::Stop, _ => Event::Start { now_ms }`.
    The production twin of this exact logic — `handle_command` in
    `src/app/session/mod.rs:241-244` — is exhaustive
    (`Status::Idle | Status::Stopped(_) => Event::Start { … }`), so the example is
    a divergent copy of code the crate already writes correctly.
  - `src/actuator/win.rs:913-916`, the test helper `sent_events`, filters the
    local `DriverCall` enum with `DriverCall::Send(event) => Some(event), _ => None`.
- **Why it matters here:** low. The preview is not shipped (`[[example]]`, dev
  only) and the test helper's intent really is "everything that is not a Send",
  which stays correct for any added variant. They are listed because they are the
  remainder of the clippy set: fixing them is what lets
  `#![warn(clippy::wildcard_enum_match_arm)]` be switched on crate-wide
  (see "Clean areas"), and leaving them behind is what will make that switch look
  noisy and get reverted.
- **Fix:** `Status::Idle | Status::Stopped(_) => Event::Start { now_ms }` in the
  example; in `win.rs` either enumerate the six non-`Send` variants or keep `_`
  and add `#[expect(clippy::wildcard_enum_match_arm, reason = "…")]` if the lint
  is enabled.
- **Effort:** trivial

### pat-005 — `watchdog.rs:103`'s wildcard is correct; do not apply clippy's suggestion to it

- **Severity:** P3
- **Rule:** [`pat-exhaustive-enum`](../../.claude/skills/rust-skills/rules/pat-exhaustive-enum.md)
- **Site:** `src/domain/control/watchdog.rs:101-104`
- **What:** clippy reports a fifth `wildcard_enum_match_arm` here and proposes
  `other @ Action::Buy { .. } | other @ Action::Recover(_) | other @ Action::Halt(_) | other @ Action::Refused(_)`:

  ```rust
  .map(|action| match action {
      Action::Refresh => Action::Recover(Recovery::Refresh),
      other => other,
  })
  ```

- **Why it matters here:** the arm is an *identity pass-through*, not a swallowed
  case — "relabel `Refresh` as a recovery re-issue, leave every other action
  alone" — and that is the right behaviour for any variant `Action` ever gains.
  Applying clippy's suggestion verbatim would make the code longer, harder to
  read, and no safer. This finding exists so that a later pass acting on the
  clippy output does not "fix" it: a reviewer who fixes pat-001 through pat-004
  and then meets this warning needs to know it was examined and kept.
- **Fix:** leave the match as it is; add one comment (e.g.
  `// Pass-through by design: only Refresh is relabelled.`) and, if the lint is
  enabled crate-wide, an `#[expect(clippy::wildcard_enum_match_arm, reason = …)]`
  carrying that sentence.
- **Effort:** trivial

### pat-006 — three wildcards over *foreign but not `#[non_exhaustive]`* enums could name their variants

- **Severity:** P3
- **Rule:** [`pat-exhaustive-enum`](../../.claude/skills/rust-skills/rules/pat-exhaustive-enum.md)
- **Site:** `src/capture/ip.rs:37`, `src/uplink/websocket.rs:204` (+ `src/config/persist.rs:248`, see below)
- **What:** the rule reserves `_` for foreign `#[non_exhaustive]` enums. Two of
  these three foreign enums are **not** marked `#[non_exhaustive]` (verified in
  the vendored sources):
  - `etherparse::NetSlice` (`etherparse-0.20.3/src/net/net_slice.rs:13`) has
    exactly `Ipv4`, `Ipv6`, `Arp`; `ip.rs:37`'s `_ => return None` is really
    "ARP is not a shop stream".
  - `tungstenite::Message` (`tungstenite-0.29.0/src/protocol/message.rs:157`) has
    `Text`/`Binary`/`Ping`/`Pong`/`Close`/`Frame`; `websocket.rs:204`'s
    `Some(Ok(_)) => {}` is really "ping/pong/frame are the library's business",
    which the comment beside it already says.
  - `toml_edit::Item`/`Value` (`persist.rs:248`) — same situation, but the
    wildcard there is over `Option<&mut Item>` with a nested `Value` pattern, so
    naming every case would be genuinely worse; recommend leaving it.
- **Why it matters here:** the payoff is bounded — adding a variant to a
  non-`non_exhaustive` public enum is a breaking change, so no 0.20.x /0.29.x
  update can spring one. But naming them converts a *future major dependency
  bump* from "the new variant is silently dropped" into a compile error at the
  one place that has to decide, which is exactly what the ADR-heavy comments in
  `pcap.rs` want for the capture path. Note clippy does **not** flag these (its
  `wildcard_enum_match_arm` reported only the five crate-local sites), so the
  lint alone will never surface them.
- **Fix:** `NetSlice::Arp(_) => return None` in `ip.rs`; in `websocket.rs`,
  `Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}`
  keeping the existing comment. Leave `persist.rs` alone.
- **Effort:** trivial

### pat-007 — one `match` that should be `let ... else` (test code)

- **Severity:** P3
- **Rule:** [`pat-let-else`](../../.claude/skills/rust-skills/rules/pat-let-else.md)
- **Site:** `src/app/mod.rs:1335-1338` (clippy-confirmed: `this could be rewritten as let...else`)
- **What:**

  ```rust
  let rejected = match budget.admit_capture(segment_with_capacity(1000, 1, 16)) {
      Ok(_) => panic!("oversized segment unexpectedly admitted"),
      Err(segment) => segment,
  };
  ```

- **Why it matters here:** purely readability, and it is the *only* site of its
  kind in 22k lines — the crate otherwise uses `let ... else` 28 times, including
  for the same "extract or diverge" shape in production code
  (`control/mod.rs:588`, `app/mod.rs:675`, `stream.rs:624`,
  `session/mod.rs:473`). Worth doing because a lone holdout is what makes
  `clippy::manual_let_else` un-enableable.
- **Fix:**

  ```rust
  let Err(rejected) = budget.admit_capture(segment_with_capacity(1000, 1, 16)) else {
      panic!("oversized segment unexpectedly admitted");
  };
  ```

- **Effort:** trivial

### pat-008 — two synthetic `(bool, Option<_>)` tuples that if-let chains make unnecessary

- **Severity:** P3
- **Rule:** [`pat-if-let-chains`](../../.claude/skills/rust-skills/rules/pat-if-let-chains.md)
- **Site:** `src/ui/journal.rs:94`, `src/ui/journal.rs:56-59`
- **What:** both build a throwaway tuple purely to combine a boolean with an
  `Option` pattern in one condition:

  ```rust
  if let (false, Some(latest)) = (open, latest) {          // :94
      …
  }

  let name = match (open, latest) {                        // :56
      (false, Some(latest)) => format!("Journal · {latest}"),
      _ => "Journal".to_owned(),
  };
  ```

- **Why it matters here:** the crate is on edition 2024 (`Cargo.toml:4`,
  `rust-version = "1.92"`) and already uses `&& let` chains in four places
  (`control/mod.rs:537` and `:577`, `app/mod.rs:457` and `:543`,
  `session/mod.rs:168`, `ui/mod.rs:231`), so this is the one file that predates
  the idiom. `(false, Some(x))` reads as a state machine over a two-element
  tuple; the chain says what is meant — "collapsed, and there is a line to
  peek". Reviewers of this file have to decode the tuple twice, 40 lines apart,
  for the same condition.
- **Fix:**

  ```rust
  if !open && let Some(latest) = latest {
      …
  }

  let name = if !open && let Some(latest) = latest {
      format!("Journal · {latest}")
  } else {
      "Journal".to_owned()
  };
  ```

  Both sit inside the `widget_info` closure / paint path, so behaviour is
  unchanged (short-circuit order is the same).
- **Effort:** trivial

## Clean areas

**`pat-exhaustive-enum` — where the crate already does it right (do not "simplify" these):**

- `src/render.rs:31`, `:71`, `:93`, `:101` — `kind_label`, `status_summary`,
  `refusal`, `describe` each enumerate every variant of `ItemKind`,
  `Status`, `RefusalReason`, `StopReason`. Adding a stop reason cannot ship
  without a player-facing wording.
- `src/ui/theme.rs:299-313` — `status_color` matches `Status` exhaustively *and*
  nests an exhaustive `StopReason` match inside `Stopped(reason)`, grouping
  variants with `|` instead of falling back. Textbook form of the rule's
  "Grouping Variants with `|`" section.
- `src/app/session/mod.rs:435` (`Action`), `:539` (`Recovery`), `:316`
  (`ServerMessage`), `:459` (`Mode`), `:518` (`SubmitError`), `:82`
  (`HaltSource`), `:238`/`:241` (`Command`, `Status`) — the whole
  decision-application layer is wildcard-free.
- `src/actuator/mod.rs:286`/`:352` (`SurfaceError`), `:327` (`Input`) and
  `src/actuator/plan.rs:170`, `:290`, `:340`, `:350`, `:398` (`Trigger`,
  `TimingPreset`, `Input`) — including the test helpers at `plan.rs:618`/`:625`,
  which spell out the unexpected variant rather than `_ => panic!`.
- `src/stream.rs:577` (`HalfOutcome`) with the comment *"Exhaustive by
  construction: a variant added to `HalfOutcome` becomes a compile error here"*;
  `:213`/`:279`/`:286` (`Stage`); `src/app/mod.rs:710`, `:719`, `:729`, `:757`,
  `:789`, `:808` (`ForwardStatus`, six exhaustive matches, no wildcard).
- `src/ui/editor/mod.rs:78-83` (`Command`) and `:507` (`Option<TimingPreset>`),
  `src/config/persist.rs:343` (`Section`), `src/uplink/websocket.rs:117`
  (`Outcome`), `src/capture/pcap.rs:354` (`LinkStrip`),
  `src/domain/control/watchdog.rs:22` (`Proof::window_ms`),
  `src/app/mod.rs:1474-1482` (test naming every `CaptureEvent` variant).
- `src/actuator/mod.rs:191-194` — the one wildcard over a genuinely
  `#[non_exhaustive]` foreign enum (`tokio::runtime::RuntimeFlavor`, verified in
  `tokio-1.52.3/src/runtime/runtime.rs:112`), and its doc comment explains why the
  fallback is correct. This is precisely what the rule's "When `_` Is Required"
  section asks for.
- Compile-time exhaustiveness by *destructuring* rather than matching:
  `Timings::named_ranges` (`plan.rs:264`) destructures all eight fields with the
  comment *"exhaustive on purpose: a ninth action added above stops compiling
  here"*, and `config.rs:409` walks its output so validation cannot skip a knob.
  Same spirit, enforced.

**`pat-matches-macro` — clean.** 74 `matches!` call sites, and
`clippy::match_like_matches_macro` reports **zero** verbose forms.
`Action::is_refusal` (`control/mod.rs:170`) is the rule's "Good for `is_*` helper
methods" pattern verbatim; the arming checks (`control/mod.rs:399`, `:425`,
`:446`, `:577`, `session/mod.rs:448`, `statusbar.rs:32`) all use `matches!` with
explicit `|` alternation rather than a negated wildcard.

**`pat-let-else` — effectively clean.** 28 `let … else` sites carrying real
invariants (`control/mod.rs:588` "not on the checklist", `app/mod.rs:528`/`:675`,
`stream.rs:624`, `session/mod.rs:473`/`:597`/`:656`, `pcap.rs:647`/`:772`/`:955`,
`persist.rs:168`/`:317`, `shop.rs:59`, `watchdog.rs:85`/`:131`,
`migrate.rs:86`, `win.rs:1361`/`:1372`/`:1381`/`:1400`). One holdout, filed as
pat-007.

**`pat-if-let-chains` — largely adopted.** Edition 2024 is set and the chains are
already used where they matter most: `control/mod.rs:537` (affordability debit),
`:577` (haul recording), `:678`-`:699` (four limit checks, each a
`if let Some(max) = … && …`), `filter.rs:70`-`:80`, `app/mod.rs:457`/`:543`,
`session/mod.rs:167`, `ui/mod.rs:230`. Two legacy sites, filed as pat-008.

**Lint that would keep this clean:** `clippy::wildcard_enum_match_arm` (the
`restriction` group) reproduces exactly the crate-owned half of this audit — five
warnings, no false positives beyond pat-005. Once pat-001 to pat-005 are settled
it can be turned on in `[lints.clippy]` and this category stops regressing on its
own. (Coordinate with the `lint-` category owner before adding it.)

## Not applicable

- **`pat-at-bindings`** — no applicable site. There is not a single range pattern
  (`n @ 1..=9`) or a value-plus-payload capture in any `match`, `if let`,
  `while let` or `let … else` in the crate: every `..=` occurrence in `src/` and
  `examples/` is a doc comment, a `DragValue::range(..)` argument, a `for` loop
  bound, or a `Range::contains` assertion — never a pattern. The enums that are
  matched (`Action`, `Status`, `Recovery`, `Trigger`, `SurfaceError`, …) are
  destructured for their payloads directly, and no arm re-accesses the scrutinee
  after matching it, so there is nothing for `@` to shorten. No finding, and no
  refactor to invent one.
- Wildcards over `Option`/`Result`, over integers (`pcap.rs:343` `for_datalink`,
  `:932` `next_ex` return codes, `app/mod.rs:1069` `parse_command` over `&str`)
  and over tuples of `Option` (`control/mod.rs:529`) are not
  `pat-exhaustive-enum` violations: `Option`/`Result` are matched exhaustively
  everywhere they appear, and `c_int`/`&str` scrutinees have no variant list a
  compiler could check.
