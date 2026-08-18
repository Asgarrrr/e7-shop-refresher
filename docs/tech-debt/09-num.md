# 09 — Numeric & Arithmetic Safety (`num-`)

**Category priority:** HIGH
**Rules audited:** 5 · **Files read:** 41 (+ `Cargo.toml`) · **Findings:** 8 (P0 0 / P1 0 / P2 5 / P3 3)

## Verdict

This category is in unusually good shape and I could not find a P0 or a P1. Every
arithmetic site that *can* reach a hostile value already handles it explicitly:
`PipelineBudget` uses `checked_add`/`saturating_sub` with a written rationale,
`DelayRange::draw` guards its own `% 0`, `Backoff::advance` is
`checked_mul(2).unwrap_or(max).min(max)`, `move_cursor` widens to `i64` *before*
subtracting and clamps before the narrowing cast, `pack_point` refuses an
out-of-word coordinate instead of masking it, and `EventLog::now_ms` is
`u64::try_from(..).unwrap_or(u64::MAX)`. The worst offender file is
`src/stream.rs`, and only by its own standard: its `release()` spells out in nine
comment lines why a byte counter must saturate rather than wrap, and then the
per-stream twin of that counter (`pending_bytes`) uses bare `-=` twice
(`stream.rs:759`, `:783`). The single highest-value fix is
[`num-002`](#num-002--pending-byte-accounting-uses-bare--where-the-modules-own-release-argues-it-must-not):
make those two subtractions saturating with a `debug_assert!`, mirroring
`release` exactly — a release-only silent corruption there degrades into a
permanent resync storm for the rest of the session, with no log line.

*Method note:* every non-test file was read in full. `src/domain/control/tests.rs`
(1781), `src/app/session/tests.rs` (1423) and the test half of
`src/config/persist.rs` were read in part and then swept with a regex for every
arithmetic operator, `as` cast, float comparison and `checked_/saturating_/
wrapping_` call; the only numerics in them are fixture id arithmetic
(`item.id += round * 100`). `cargo clippy --all-targets` with
`cast_possible_truncation`, `cast_sign_loss`, `cast_lossless`,
`cast_possible_wrap`, `float_cmp` and `arithmetic_side_effects` enabled produced
62 lib warnings + 18 test warnings; every site below is cited from that run, and
every site in that run that is *not* below was checked and dismissed.

## Findings

### num-001 — a float inside `Filter`'s derived `PartialEq` is the Apply dirty-check; a `nan` threshold makes it permanently dirty

- **Severity:** P2
- **Rule:** [`num-float-compare`](../../.claude/skills/rust-skills/rules/num-float-compare.md)
- **Site:** `src/ui/editor/mod.rs:623` (the comparison) · `src/domain/filter.rs:46-52` (`SubstatReq { min: Option<f64> }`, `#[derive(PartialEq)]`) · `src/domain/filter.rs:131` (`value >= min`) · `src/config.rs:344-424` (`validate`, which never inspects `required_substats`)
- **What:** `let dirty_filter = editor.filter != editor.applied_filter;` is a
  float `!=` in disguise: the derived `PartialEq` on `Filter` recurses into
  `Vec<SubstatReq>` and compares `Option<f64>` with `==`. `Config::validate`
  bounds `game_port`, `server_url`, `filter.kinds` and all eight
  `[actuator.timings]` ranges, but never checks that a `required_substats[].min`
  is finite — and TOML 1.0 has `nan`/`inf` float literals, so
  `min = nan` in `config.toml` is a legal parse. Reproduced against the real
  `toml`/`serde` versions from `Cargo.lock`:

  ```text
  parsed: Filter { required_substats: [SubstatReq { name: "speed", min: Some(NaN) }] }
  f == f.clone(): false
  ```

  Note that clippy's `float_cmp` does **not** catch this: it fires on a literal
  `==` between floats, not on a derived `PartialEq` that contains one. In this
  crate `float_cmp` only fires on `plan.rs:780-781` (test asserts on exact
  binary-representable `f32` constants — fine).
- **Why it matters here:** two symptoms, both silent. (1) The window's Apply
  button is lit forever and the commit bar permanently reads "Hunt edited",
  because `mark_applied` copies the same `NaN`-bearing filter into
  `applied_filter` and `NaN != NaN`; every click re-sends `SetFilter` and
  rewrites `config.toml` (`ui/mod.rs:229-236`). (2) `value >= min` is `false` for
  every value, so the filter can never match anything — while
  `is_unrestricted()` returns `false`, so the loop happily arms and burns
  crystals refreshing forever. The `+inf` case shares symptom (2) without
  symptom (1).
- **Fix:** reject non-finite thresholds at the root, in the same loop that
  already rejects reversed timing ranges:

  ```rust
  // in Config::validate, beside the named_ranges loop
  for req in &self.filter.required_substats {
      if req.min.is_some_and(|min| !min.is_finite()) {
          return Err(crate::Error::Config(format!(
              "filter.required_substats \"{}\" has a non-finite min — no substat value can \
               ever satisfy it, so the filter would match nothing", req.name)));
      }
  }
  ```

  and add one line above `dirty_filter` documenting that the comparison is
  deliberately bit-exact (change detection since last write), which is what
  `num-float-compare` asks for when `==` on floats is kept on purpose.
- **Effort:** trivial

### num-002 — pending-byte accounting uses bare `-=` where the module's own `release()` argues it must not

- **Severity:** P2
- **Rule:** [`num-overflow-explicit`](../../.claude/skills/rust-skills/rules/num-overflow-explicit.md)
- **Site:** `src/stream.rs:759` (`self.pending_bytes -= old.capacity();`) and `src/stream.rs:783` (`self.pending_bytes -= payload.capacity();`); the matching `+=` at `:771` and `:470`
- **What:** `HalfStream::pending_bytes` is the per-stream twin of
  `Usage::{total,capture,reassembly,outbound}`. For the latter,
  `PipelineBudget::release` (`stream.rs:182-210`) carries an explicit nine-line
  argument for `saturating_sub` + `debug_assert!` over a bare `-`, plus two tests
  (`release_underflow_saturates_instead_of_panicking`,
  `release_underflow_fails_fast_in_debug_builds`) pinning both profiles. The
  per-stream counter got neither. I traced the invariant and it currently holds —
  `capacity()` returns the immutable `lease.bytes`, so it does not drift when
  `payload.bytes.drain(..already)` shrinks the `Vec`, and both subtraction sites
  are preceded by a `remove`/`pop_first` of an entry whose capacity was added at
  insert — so this is debt, not a live bug.
- **Why it matters here:** the two profiles fail differently and both fail badly,
  which is exactly what the rule is about. **Release: silent corruption, not a
  wrong number.** An underflow wraps `pending_bytes` to ~1.8e19, so
  `pending_bytes.checked_add(capacity).is_none_or(|b| b > MAX_PENDING_BYTES)`
  (`:763-766`) is true forever, `buffer_future` returns `false` forever,
  `HalfOutcome::Pressure` clears every flow's anchor on every out-of-order
  segment, and the session spends the rest of its life re-anchoring — visible to
  the player only as a shop that stops updating. **Debug: a panic inside the
  reassembly task**, which `SessionWorkers::spawn`'s `catch_unwind` turns into
  `"reassembly task panicked"` and a dead session. Neither is a diagnosable
  "accounting bug" message the way `release`'s is.
- **Fix:** mirror `release` verbatim at both sites:

  ```rust
  let released = old.capacity();
  if self.pending_bytes < released {
      error!(pending_bytes = self.pending_bytes, released, "pending accounting underflow");
      debug_assert!(self.pending_bytes >= released, "pending accounting underflow");
  }
  self.pending_bytes = self.pending_bytes.saturating_sub(released);
  ```

  (or a small `fn release_pending(&mut self, bytes: usize)` used by both).
- **Effort:** small

### num-003 — the gold-balance debit is a bare `u32` subtraction of two wire-supplied values

- **Severity:** P2
- **Rule:** [`num-overflow-explicit`](../../.claude/skills/rust-skills/rules/num-overflow-explicit.md)
- **Site:** `src/domain/control/mod.rs:537-539`
- **What:**

  ```rust
  if in_reach && let (Some(balance), Some(price)) = (gold, item.price) {
      gold = Some(balance - price);
  }
  ```

  Both operands come off the wire: `balance` is seeded from a `purchase` echo's
  `gold` field and `price` from a shop slot. The subtraction is safe *today*
  only because `in_reach` implies `affordable`, which was computed as
  `price <= balance` eight lines up (`:529-532`) from the same two bindings —
  a non-local invariant that a reordering, an added `in_reach` term, or a future
  "reserve some gold" tweak would break silently. It is the only bare arithmetic
  operator in a module that otherwise uses `saturating_add` for `refreshes`,
  `spent`, `matches_found` and the `Haul` counters, `saturating_sub` for the
  crystal debit and `now_ms`, and `checked_add` for the `max_spend` ceiling.
- **Why it matters here:** release-only wrap → `gold` becomes ~4.29e9 → every
  remaining match in the same snapshot reads as affordable → the actuator clicks
  Buy on items the player cannot pay for, which the domain then waits on as an
  unresolvable checklist entry (the purchase echo never arrives) until the
  watchdog halts with `Unresponsive`. Debug → panic inside `plan_targets`, i.e.
  inside the controller mutex, poisoning it for the GUI.
- **Fix:** `gold = Some(balance.saturating_sub(price));`. Saturating is the right
  choice, not checked: reaching zero and refusing the next item is exactly the
  intended semantics. `gold_debits_cumulatively_within_one_shop`
  (`domain/control/tests.rs:788`) already covers the happy path.
- **Effort:** trivial

### num-004 — the `WM_MOUSEWHEEL` delta is packed with a silent truncation, in the file whose `pack_point` refuses exactly that

- **Severity:** P2
- **Rule:** [`num-cast-try-from`](../../.claude/skills/rust-skills/rules/num-cast-try-from.md)
- **Site:** `src/actuator/win.rs:690-696` (`((delta as u32) << 16) as usize`); contrast `src/actuator/win.rs:705-726` (`pack_point`)
- **What:** `WM_MOUSEWHEEL`'s `wParam` high word is a *signed 16-bit* delta.
  `delta` is `notches.saturating_mul(WHEEL_DELTA)` — an `i32` — and
  `(delta as u32) << 16` throws away everything above bit 15 with no diagnostic
  (`<<` only panics on an over-wide *shift amount*, never on discarded bits, so
  this is silent in debug too). Twelve lines below, `pack_point` refuses an
  out-of-`i16` coordinate with a `SurfaceError::Recoverable` and a doc comment
  explaining that masking it "would fold it silently onto some other pixel and
  the click would still be posted". The wheel path takes the option that comment
  rejects. Clippy flags the same line (`cast_sign_loss` at `win.rs:694`).
- **Why it matters here:** not currently reachable — every `Input::Scroll` in the
  crate carries `±SCROLL_TO_EXTREME_NOTCHES` (`plan.rs:34`, i.e. `±1200` after
  the multiply, well inside `i16`) — so this is a robustness/consistency defect,
  not a live bug. It matters because `Surface::scroll(&mut self, at, notches: i32)`
  is the trait boundary the actuator is extended through, and the failure mode of
  getting it wrong is a wheel event scrolling the wrong distance in the *opposite*
  direction while every layer reports success (`SendInput`/`PostMessageW` cannot
  report a wrong-but-well-formed delta).
- **Fix:** validate like the coordinate sibling, so the two paths in one file
  agree:

  ```rust
  let delta = i16::try_from(notches.saturating_mul(WHEEL_DELTA)).map_err(|_| {
      SurfaceError::Recoverable(format!("wheel delta for {notches} notches out of wParam range"))
  })?;
  post(target.hwnd, WM_MOUSEWHEEL, (u32::from(delta.cast_unsigned()) << 16) as usize, pack_point(at)?)?;
  ```

  While there, `send_mouse(data: i32)`'s `mouseData: data as _` (`win.rs:484`)
  hides its target type behind `as _`; spell it out.
- **Effort:** small

### num-005 — the release profile leaves `overflow-checks` off, so ~35 bare arithmetic sites wrap without a diagnostic in the shipped build

- **Severity:** P2
- **Rule:** [`num-overflow-explicit`](../../.claude/skills/rust-skills/rules/num-overflow-explicit.md)
- **Site:** `Cargo.toml:112-118` (`[profile.release]`); no `[lints]` table anywhere in the manifest
- **What:** `clippy::arithmetic_side_effects` counts 35 bare `+`/`-`/`*`/`%`
  sites in the lib. Most are provably bounded (see *Clean areas*), but the
  shipped profile turns any that is not into a wrapped value with no panic, no
  log, and no `crash.log` entry — while the same code in `cargo test` panics.
  The crate has gone to real trouble to make faults observable (`crash.rs`'s
  panic hook, `catch_unwind` per worker in `SessionWorkers::spawn`, a `fatal`
  channel into the session loop, `strip = "debuginfo"` kept over
  `strip = true` specifically so backtraces stay symbolised) — and integer
  overflow is the one class of fault that bypasses all of it.
- **Why it matters here:** the arithmetic in this crate is on values the crate
  does not control (TCP payload lengths, sequence deltas, wire prices and gold
  balances, `GetSystemMetrics` results, config-file millisecond counts). A wrap
  in any of them is a wrong click or a stalled pipeline that the player reports
  as "it stopped working", with a log file that shows nothing at all.
- **Fix:** two options, and the decision should be *recorded* either way rather
  than left implicit.
  1. `[profile.release] overflow-checks = true`. Cost is a few percent on
     integer-heavy loops (irrelevant here — the hot path is `pcap_next_ex` plus a
     `BTreeMap`), benefit is that every finding above becomes a `crash.log` entry
     instead of silent corruption. **Caveat that must be checked first:**
     `stream.rs:182-191` documents that a panic raised from a `Drop` during an
     unwind aborts the process with no banner and no `crash.log`. `PayloadLease::drop`
     → `PipelineBudget::release` is already panic-free by construction
     (`saturating_sub`), but enabling the flag globally means auditing every other
     `Drop` in the crate for arithmetic first. There is little: `Drop` impls live in
     `stream.rs` (`PayloadLease`), `capture/pcap.rs` (`Handle`, `PcapSource`),
     `actuator/mod.rs` (`SurfaceJobGuard`), `actuator/win.rs` (`MessageSurface`),
     and none of them does arithmetic today.
  2. Leave it off and say so in a comment next to `codegen-units = 1`, on the
     grounds that the counters are all `u64` and the deliberate `saturating_*`
     calls are the real defence. This is a defensible position — but it is only
     defensible once `num-002`, `num-003` and `num-004` are fixed.
- **Effort:** trivial (option 1: one line + the `Drop` audit above, which this
  finding has already done)

### num-006 — `as` where a `From`/`TryFrom` is available: 8 sites

- **Severity:** P3
- **Rule:** [`num-cast-try-from`](../../.claude/skills/rust-skills/rules/num-cast-try-from.md)
- **Site:** `src/stream.rs:835` (`(a.wrapping_sub(b) as i32) as i64` — the outer cast is `i64::from`; clippy `cast_lossless`, the one suggestion `clippy --fix` offers) · `src/stream.rs:741` (`payload.as_slice().len() as i64`) · `src/capture/pcap.rs:953` (`caplen as usize`) · `src/capture/pcap.rs:405, 1169, 1171` (`SNAPLEN as c_uint`) · `src/domain/control/mod.rs:492` (`targets.len() as u32`) · `src/ui/editor/mod.rs:190, 196` (`n as usize`) · `src/actuator/win.rs:865` (`duration.as_millis() as u64`, test recorder)
- **What:** each is a cast whose truncation is unreachable in practice but which
  is written as an unchecked `as` on a value derived from the wire or from a
  `len()`. `stream.rs:741` and `pcap.rs:953` are the two that touch wire-derived
  lengths: `len() as i64` and `caplen as usize` are same-width reinterpretations
  on x86-64 (`usize`↔`i64`, `c_uint`→`usize`), bounded to ≤ `SNAPLEN` (262 144)
  by `plausible_caplen`. `control/mod.rs:492` narrows a slot count to `u32`.
- **Why it matters here:** none of these is a bug; the value is that the *shape*
  of the code stops distinguishing "checked narrowing" from "unchecked
  narrowing", which is what makes `num-004`-style mistakes easy to add later.
  This crate documents its deliberate casts (see *Clean areas*) — these are the
  ones that read as accidental because they carry no note.
- **Fix:** `i64::from(..)` for the lossless ones; `usize::try_from(caplen).unwrap_or(0)`
  / `i64::try_from(len).unwrap_or(i64::MAX)` / `u32::try_from(targets.len()).unwrap_or(u32::MAX)`
  for the narrowing ones (each already has a correct saturating answer). Do this
  when touching the file, not as a sweep.
- **Effort:** trivial

### num-007 — two float→int casts on values whose range is not verified at the cast

- **Severity:** P3
- **Rule:** [`num-cast-try-from`](../../.claude/skills/rust-skills/rules/num-cast-try-from.md)
- **Site:** `src/actuator/plan.rs:152-153` (`(rect.left as f32 + off_x + x * s).round() as i32`, `(rect.top as f32 + point.y * s).round() as i32`) · `src/ui/editor/timing_meter.rs:187` (`(target_ms - baseline as f32).clamp(0.0, headroom).round() as u64`)
- **What:** `as` from float to int saturates at the bounds but maps `NaN` to `0`.
  I verified both are `NaN`-free: `to_screen` rejects `rect.width <= 0 || rect.height <= 0`
  first so `ch >= 1.0` and no `0.0/0.0` is possible, and `slack_from_target`'s
  `frac` comes from a bar whose width is floored at 80.0 (`editor/mod.rs:92`) so
  its division cannot produce `NaN` either. The rule permits `as` here — "float-
  to-integer when you have verified the range **and documented the intent**" —
  and the documentation is what is missing at both sites. `timing_meter.rs`
  documents the adjacent `clamp` panic guard in detail but not the cast;
  `plan.rs` documents neither.
- **Why it matters here:** `to_screen`'s output is a click coordinate. A `NaN`
  reaching it would produce `(0, 0)` — a click at the top-left of the screen,
  outside the game window, silently — which is precisely the class of failure the
  rest of this file is built to prevent (`MAX_ASPECT` refusal, `pack_point`
  refusal, `Target::verify`). The invariant is real; it is just invisible.
- **Fix:** one comment at each site naming the guard that makes the range safe
  (`rect.width/height > 0` checked above ⇒ `s` finite ⇒ no `NaN`; the ruler
  clamp bounds `slack` to `0..=RULER_MS`). No code change.
- **Effort:** trivial

### num-008 — the two computed divisors and the retry counter are kept legal by a comment rather than by a type

- **Severity:** P3
- **Rule:** [`num-nonzero`](../../.claude/skills/rust-skills/rules/num-nonzero.md), [`num-overflow-explicit`](../../.claude/skills/rust-skills/rules/num-overflow-explicit.md)
- **Site:** `src/actuator/plan.rs:201-214` (`jitter.next() % modulus`) · `src/actuator/win.rs:459-465` (`* 65_535 / i64::from(width)`, same for `height`) · `src/domain/control/watchdog.rs:53, 61, 69` (`now_ms + proof.window_ms()`, `self.attempt + 1`)
- **What:** I checked every `/` and `%` in the crate for a zero divisor on a
  reachable path. All but two divide by a non-zero constant (`1000`, `3600`,
  `60`, `3`, `51`, `60_000`, `RULER_MS`, `2.0`) and are safe by inspection. The
  two computed ones are:
  - `DelayRange::draw`'s `modulus = span.checked_add(1)`, non-zero because
    `span == 0` returned early — carrying a four-line comment about why `% 0`
    would otherwise panic, plus the test `full_u64_range_draws_without_panicking`.
  - `move_cursor`'s `width`/`height`, non-zero because of
    `system_metric(..).max(1)` — carrying a comment about a failed
    `GetSystemMetrics` reading 0.

  Both are *correct*; both encode the non-zero-ness in prose and a guard rather
  than in `NonZeroU64` / `NonZeroI32`, which is what the rule asks for. Separately,
  `Expectation`'s `attempt: u8` is incremented with a bare `+ 1` whose bound is
  the `match (proof, attempt)` arms in `Controller::watchdog` — `escalate` is only
  called from the `0` and `1` arms, and the `_` arm halts and clears the
  expectation, so `attempt` never exceeds 2. Correct, and again by non-local
  reasoning.
- **Why it matters here:** a divide-by-zero is an unconditional panic in both
  profiles, and `move_cursor` runs on a runtime worker inside `block_in_place`
  while `draw` runs in the session loop — either panic ends a session. The guards
  are right; the types would make them unremovable.
- **Fix:** where the value already has a natural home, thread a `NonZero*`:
  `fn draw(&self, ..) -> u64 { ... let modulus = NonZeroU64::new(span.wrapping_add(1))...; jitter.next() % modulus }`
  (`%` is implemented for `NonZeroU64` divisors) and
  `let width = NonZeroI32::new(system_metric(SM_CXVIRTUALSCREEN)).unwrap_or(NonZeroI32::MIN)`
  — or, more cheaply, `let width = system_metric(..).max(1)` kept as is with a
  `NonZeroI32::new(..).expect(..)` at the boundary. `attempt.saturating_add(1)`
  is a free one-word change. This is a nit: do it when touching the file.
- **Effort:** small

## Clean areas

**`num-overflow-explicit` — done right, and worth not "simplifying":**
- `src/stream.rs:146-210` — `PipelineBudget::reserve_new`/`try_retag`/`release`:
  `checked_add` for every admission, `saturating_sub` + `debug_assert!` for every
  release, and a hard `assert!` in `try_retag` (which never runs from a `Drop`).
  The comment at `:182-191` explaining why a `Drop`-reachable underflow must not
  panic is the best piece of numeric reasoning in the crate; the two profile-split
  tests at `:950-980` pin it.
- `src/stream.rs:222-245` — `record_drop`/`record_resync`: `saturating_add` inside
  `fetch_update`, and `u64::try_from(bytes).unwrap_or(u64::MAX)` for the
  `usize`→`u64` widening.
- `src/stream.rs:457-463` — `InitialBurst::would_exceed`: `checked_add(..).is_none_or(..)`,
  and `push` asserts it was called first.
- `src/uplink/websocket.rs:49-55` — `Backoff::advance`: `checked_mul(2).unwrap_or(self.max).min(self.max)`,
  with `backoff_growth_caps_without_overflow` testing `Duration::MAX - 1ns`.
- `src/actuator/plan.rs:201-214` — `DelayRange::draw`: `saturating_sub` for the
  span, `checked_add` for the modulus with a documented `None` branch,
  `saturating_add` for the result.
- `src/domain/control/mod.rs:489-492, 642-648, 677-716` — `saturating_add` on
  `refreshes`/`spent`/`matches_found`, `saturating_sub` on `crystal_balance`,
  `checked_add(..).is_none_or(..)` for the `max_spend` ceiling, `saturating_sub`
  on the elapsed-duration check with the underflow reasoning in the doc comment.
- `src/domain/control/mod.rs:203-228` — `Haul`: `fold(0u32, u32::saturating_add)`
  and `saturating_add` per record.
- `src/ui/editor/mod.rs:525-541` — `pass_estimate`: `fold(base, u64::saturating_add)`
  rather than `sum::<u64>()`, with the comment explaining exactly why.
- `src/ui/editor/timing_meter.rs:131-137, 196-198` — `saturating_add` for both
  the painted total and the readout, with the "panic in debug and, worse, wrap
  silently in release" comment stating the two profiles by name. This is the
  crate teaching the rule back at me.
- `src/ui/editor/mod.rs:354-376` — the duration rail: `div_ceil(60_000)`,
  `saturating_mul(60_000)`, and a `DragValue` range of `1..=u64::MAX / 60_000` so
  the multiply cannot leave range in the first place.
- `src/journal.rs:44-46` — `u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)`.
- `src/config.rs:39, 409-422` — `MAX_TIMING_MS`: a validation ceiling introduced
  specifically to keep `baseline + extra` sums away from `u64::MAX`, with the
  reasoning written out and eight tests (`every_timing_range_is_checked_not_just_the_first`).
- `src/domain/shop.rs:122-128` — `effective_slot`: `u8::try_from(index + 1).unwrap_or(u8::MAX)`,
  with a doc comment about not wrapping back onto the `0` sentinel.
- `src/actuator/plan.rs:90-92` — `row_for_slot`: `slot.checked_sub(1).filter(..)`.
- `src/render.rs:43-54` — `grouped`: `len - 1` and `len - index` are both safe
  because `u32::to_string()` is never empty and `index < len`; `is_multiple_of`
  instead of `% 3 == 0`.
- `src/app/session/mod.rs:148, 204` — `ticks.wrapping_add(1)` (explicitly
  wrapping, correct for a heartbeat counter) and `saturating_sub(at) / 1000`.

**`num-cast-try-from` — the casts that are load-bearing and correctly so:**
- `src/actuator/win.rs:456-478` — `move_cursor`: widens to `i64` *before*
  subtracting (the comment says why: `SM_XVIRTUALSCREEN` goes negative), then
  `clamp(0, 65_535)` *before* the narrowing `as i32`. The rule's textbook answer.
- `src/actuator/win.rs:705-726` — `pack_point`: `i16::try_from` on both
  coordinates with an error rather than a mask, `u32::from` for the widening, and
  a doc comment explaining why the assembly must go through `u32` and not `i32`.
  Round-tripped through `GET_X/Y_LPARAM` by `pack_point_round_trips_through_get_x_y_lparam`.
- `src/stream.rs:726-736` — `usize::try_from(self.next_off - offset)` with an
  `error!` + `debug_assert!` on the impossible branch, and a five-line comment
  spelling out that an `as` cast there would silently produce ~1.8e19 and freeze
  the half-stream. Exactly the finding this rule exists to prevent, already
  prevented.
- `src/stream.rs:794-802` — `(self.next_off as u64) as u32`: documented modular
  truncation into the TCP sequence space, with the detour through `u64` explained.
- `src/stream.rs:833-836` — `seq_diff`'s `as i32`: the canonical wrap-safe TCP
  sequence comparison; no `From` exists for a bit reinterpretation.
- `src/watch.rs:37, 102, 132` — `source as u8` on a `#[repr(u8)]` bitflag enum,
  with the "each discriminant is a distinct bit" doc comment.
- `ERROR_* as i32` at `win.rs:432, 546, 1349`, `shield.rs:117, 215, 291`,
  `migrate.rs:196, 278`, `actuator/mod.rs:741` — the `WIN32_ERROR`(`u32`) →
  `io::Error::from_raw_os_error`(`i32`) boundary; clippy's `cast_signed()`
  suggestion is cosmetic and these read fine as they are.
- `src/ui/shop.rs:34`, `src/ui/journal.rs:32-33`, `src/ui/mod.rs:189` — `f32::from(i8/u8)`
  rather than `as f32`.
- `src/actuator/plan.rs:447-449` — `Jitter::unit`: `(next() >> 40) as f32 / (1u64 << 24) as f32`
  keeps the mantissa at 24 bits, so both casts are exact.
- `src/capture/ip.rs` — no casts at all: `etherparse` hands back typed accessors
  and `parse_segment` propagates them unchanged. The one file the brief expected
  to be full of narrowing casts on wire lengths has none.

**`num-saturating-clamp`:** `move_cursor`'s `clamp(0, 65_535)`;
`system_metric(..).max(1)`; `Config::reconnect_initial/max`'s
`.max(RECONNECT_FLOOR)` / `.max(self.reconnect_initial())` (order enforced by
`reconnect_durations_enforce_floor_and_order`); `Backoff::new`'s
`initial.max(RECONNECT_FLOOR)` and `max.max(initial)` — so `Backoff` can never be
constructed with `min > max`; `timing_meter`'s `frac.clamp(0.0, 1.0)`,
`headroom.max(0.0)` before the `clamp` (with the `f32::clamp` `min > max` panic
named in the comment) and `grip_x.clamp(..)`; `ui/mod.rs:177`'s
`(height - 160.0).max(80.0)` floor before building a `Rangef`;
`editor/mod.rs:92`'s `.max(80.0)` bar width. The rule's `min > max` footgun is
handled at every site that could hit it.

**`num-float-compare`:** no `sort_by(|a, b| a.partial_cmp(b).unwrap())` anywhere —
the one float-ordering site, `InitialBurst::into_ordered` (`stream.rs:492-533`),
sorts integers via `seq_diff` against a reduced origin, with a comment on why a
transitive key is needed. `SubstatReq::satisfied_by` uses `>=`, not `==`.
`TimingPreset::from_timings` compares `Timings`, which is all-`u64` and derives
`Eq`. `theme.rs` compares only `Color32`. The only `float_cmp` hits in the whole
crate are two test assertions on exact `f32` constants (`plan.rs:780-781`).

**`num-nonzero`:** every `/` and `%` other than the two in `num-008` divides by a
non-zero literal constant; there is no reachable division by an unvalidated
value. `Jitter::new` remaps a zero seed to an odd constant so the xorshift state
can never be zero — the same "forbid zero at construction" instinct the rule
describes, hand-rolled.

## Not applicable

- **`num-nonzero`'s niche-optimization half** — the crate has no
  `Option<integer-id>` field where `size_of::<Option<T>> == size_of::<T>()` would
  matter for the ones that count. The three types that *are* size-budgeted
  (`FlowKey`, `Segment`, `BudgetedSegment`, `BudgetedChunk`) already have
  `const _: () = assert!(size_of::<..>() == ..)` canaries at `capture/mod.rs:77-83`
  and `stream.rs:418-424`, and none of them carries an `Option<u32>`. The
  `Option<u32>` fields that do exist (`ShopItem::price`, `grade`,
  `Limits::max_*`, `Controller::gold_balance`) live behind a network hop or in a
  config struct; `0` is a meaningful value for most of them (`max_refreshes = 0`
  halts at the first check, and `seeded_zero_limit_is_not_silently_clamped`
  deliberately preserves it), so `NonZeroU32` would be semantically *wrong*
  there, not merely unnecessary.
- **`num-overflow-explicit` on the monotonic counters** — `Reassembler::clock`
  (`stream.rs:560`), `Funnel::{delivered,unparsed,admitted}` (`pcap.rs:641-653`),
  `capture_loop`'s `delivered` and `admitted_segments` (`pcap.rs:954`,
  `app/mod.rs:970`) are all `u64` incremented once per captured packet. At a
  sustained 10⁹ packets/second they overflow in 584 years. Clippy flags them
  under `arithmetic_side_effects`; they are not findings, and changing them to
  `saturating_add` would add noise without adding safety. Noted here so a later
  reader does not "fix" 6 sites for nothing.
