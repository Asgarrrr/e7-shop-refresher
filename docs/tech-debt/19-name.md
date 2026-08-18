# 19 — Naming Conventions (`name-`)

**Category priority:** MEDIUM
**Rules audited:** 16 · **Files read:** 42 (39 `.rs` under `src/`, plus `examples/ui_preview.rs`, `build.rs`, `Cargo.toml`) · **Findings:** 5 (P0 0 / P1 0 / P2 2 / P3 3)

## Verdict

This crate is close to clean on naming, and most of my rule set is either
compiler-enforced or inapplicable here. The four case rules
(`name-types-camel`, `name-variants-camel`, `name-funcs-snake`,
`name-consts-screaming`) are guaranteed by `rustc`'s warn-by-default lints:
there is **no** `#[allow(non_snake_case)]`, `#[allow(non_camel_case_types)]` or
`#[allow(non_upper_case_globals)]` anywhere in the tree (grepped), and
`cargo clippy --all-targets` finishes with zero diagnostics — so I did not
re-audit what the compiler guarantees, I verified the escape hatches are absent.
The three iterator rules have nothing to bind to: this crate exposes no
iterator method and no iterator type. The conversion prefixes (`as_`/`to_`/
`into_`) are used correctly at every one of the seven sites that use them.

The two findings that matter are both about a name that says less than the
thing does: `EventLog::entries()` (`src/journal.rs:100`) is a plain-noun getter
that deep-clones up to 500 `String`s per call — the GUI had to grow a
`generation()` counter specifically to stop calling it every frame, which is
the evidence the name misled — and `WatchGate::halt_requested()`
(`src/watch.rs:110`) reads like a cheap `bool` predicate but is an `async`
wait that parks until a cause exists. **The single highest-value fix is
renaming `EventLog::entries()` to `to_entries()`** (or returning a guard):
one rename, and the only allocating "getter" on a hot path stops advertising
itself as free. Worst offender file, on volume of nits: `src/domain/control/mod.rs`
(two of the four unprefixed `bool` predicates).

## Findings

### name-001 — `EventLog::entries()` deep-clones the whole journal behind a free-getter name

- **Severity:** P2
- **Rule:** [`name-to-expensive`](../../.claude/skills/rust-skills/rules/name-to-expensive.md)
- **Site:** `src/journal.rs:100` (the two callers that matter: `src/ui/mod.rs:117`, `src/ui/mod.rs:144`; 40 call sites in total, the rest test assertions)
- **What:** `pub fn entries(&self) -> Vec<LogLine>` locks the ring buffer and
  `.iter().cloned().collect()`s it — up to `JOURNAL_CAP` (500) `LogLine`s, each
  owning a `String`. The name is a bare noun, the shape std reserves for free
  field access (`Vec::len`, `Path::file_name`). The rule's Bad example is
  exactly this: `fn get_string(&self) -> String { self.0.clone() }` — "misleading:
  suggests cheap reference".
- **Why it matters here:** the window repaints at 4 Hz and this is the only
  journal read it has. The cost was discovered the hard way: `EventLog` carries
  a whole extra `generation: Arc<AtomicU64>` field, and `ShopApp` a
  `journal_cache` + `journal_generation` pair, whose documented purpose is
  "re-cloned only when the generation changes: the journal grows at human pace,
  repaints at display rate" (`src/ui/mod.rs:91-93`). A name that priced the
  call correctly would have made that caching obvious at the call site instead
  of a discovery. It also invites the next reader to call `entries()` inside a
  render closure.
- **Fix:** rename to `to_entries()` (one prefix, keeps the noun, signals the
  allocation), or return a `MutexGuard`-backed borrow and leave the copying to
  the caller. 40 call sites, all but two of them test assertions. Same rule, far lower stakes,
  in the same pass if convenient: `CaptureConfig::retired_keys()` /
  `ForwardConfig::retired_keys()` (`src/config.rs:225`, `src/config.rs:243`)
  also allocate (a `Vec` plus a joined `String`) behind a plain noun — but they
  run once per launch, so leaving them alone is defensible.
- **Effort:** trivial

### name-002 — `WatchGate::halt_requested()` reads as a predicate, but blocks and returns the cause

- **Severity:** P2
- **Rule:** [`name-is-has-bool`](../../.claude/skills/rust-skills/rules/name-is-has-bool.md) (spirit: the name must answer the question it looks like it asks) — filed here as a misleading name, not a prefix nit
- **Site:** `src/watch.rs:110`; the one production caller is `src/app/session/mod.rs:81`, the other 15 call sites are tests (`src/watch.rs`, `src/actuator/mod.rs`, `src/ui/mod.rs:469`)
- **What:** `pub async fn halt_requested(&self) -> HaltSource` loops on
  `notified().await` until a cause is latched, then returns it without
  consuming it. Sitting next to `request_halt(&self, source)`, the past
  participle reads as "has a halt been requested?" — i.e. a cheap `bool` check.
  It is the opposite: a suspend point that never returns until something halts.
- **Why it matters here:** the one production caller is a
  `tokio::select! { biased; source = gate.halt_requested() => ... }` arm in the
  session loop. Reading that name as a poll makes the whole loop's control flow
  look wrong (a predicate as the first biased branch would be a spin), and
  reading it as a poll *elsewhere* — say, in the GUI's per-frame `ui()` — would
  hang the window. The doc comment ("Waits for a pending cause without
  consuming it") is doing all the work the name should.
- **Fix:** rename to `next_halt()` or `wait_for_halt()`. 16 call sites, 15 of
  them one-line test assertions. If a genuine predicate is ever wanted,
  `is_halt_pending()` is then free to mean what it says.
- **Effort:** trivial

### name-003 — Four `bool`-returning predicates without an `is_`/`has_`/`can_` prefix

- **Severity:** P3
- **Rule:** [`name-is-has-bool`](../../.claude/skills/rust-skills/rules/name-is-has-bool.md)
- **Site:** collapsed, 4 sites:
  - `src/domain/control/mod.rs:311` — `pub fn recovery_enabled(&self) -> bool` → `is_recovery_enabled`
  - `src/domain/control/mod.rs:712` — `fn duration_elapsed(&self, now_ms: u64) -> bool` → `has_duration_elapsed`
  - `src/app/mod.rs:123` — `fn blocks_segments(&self) -> bool` → `is_blocking_segments`
  - `src/capture/pcap.rs:404` — `fn plausible_caplen(caplen: c_uint) -> bool` → `is_plausible_caplen`
- **What:** each answers a yes/no question about state and each omits the
  prefix the rule requires. The rule's own Bad example is `fn active(&self) -> bool`
  read at a call site as `if user.active()` — "is this checking or activating?".
  Here `recovery_enabled()` is the closest analogue: it sits one method below
  `enable_recovery(&mut self)`, so `ctrl.recovery_enabled()` and
  `ctrl.enable_recovery()` differ by one word and one of them mutates.
- **Why it matters here:** `plausible_caplen` is the FFI layout canary — the
  one function standing between a mis-declared `timeval` and a session that
  slices garbage — and it is called as `if !plausible_caplen(caplen)`, where the
  reader has to know it is a test and not a coercion. `duration_elapsed` reads
  as a statement of fact inside `stop_reason`. None of these is a defect today;
  they are a one-pass rename that makes four call sites self-describing.
- **Fix:** rename as listed above. `recovery_enabled` is `pub` but internal
  (`publish = false`), and its only callers are `src/app/mod.rs`'s two
  `setup_enables_recovery_only_when_live` tests, so the rename is contained.
- **Effort:** trivial

### name-004 — `SubStat` and `SubstatReq` case the same word two different ways

- **Severity:** P3
- **Rule:** [`name-types-camel`](../../.claude/skills/rust-skills/rules/name-types-camel.md) (word-boundary consistency; see also [`name-acronym-word`](../../.claude/skills/rust-skills/rules/name-acronym-word.md))
- **Site:** `src/domain/shop.rs:143` (`pub struct SubStat`) vs `src/domain/filter.rs:48` (`pub struct SubstatReq`)
- **What:** `SubStat` declares "sub" and "stat" to be two words; `SubstatReq`
  declares "substat" to be one. Every other identifier in the crate takes the
  second reading: the wire/serde field `substats` (`src/domain/shop.rs:95`),
  `Filter::min_substats`, `Filter::required_substats`,
  `EditorState::substat_input`, `fn substat_reqs`, and the test helper
  `fn substat(...)`. So the odd one out is the type, and the two types are
  routinely imported side by side (`use crate::domain::shop::{PurchaseLimit, SubStat}`
  in `src/domain/filter.rs`'s tests, `SubStat` + `SubstatReq` in `src/ui/editor/mod.rs`'s import block).
- **Why it matters here:** it is the only place in the crate where the same
  domain noun has two spellings, so it is the only place where "which
  capitalisation was it?" is a real question — and `rustc` cannot help, both
  are valid `UpperCamelCase`.
- **Fix:** rename `SubStat` → `Substat`. Three files touched
  (`src/domain/shop.rs`, `src/domain/filter.rs`, `src/domain/control/dedup.rs`),
  no public API outside this binary.
- **Effort:** trivial

### name-005 — Three helper names that say nothing about what they return, in modules that otherwise name by role

- **Severity:** P3
- **Rule:** general misleading/under-specified naming (no single rule file; adjacent to [`name-no-get-prefix`](../../.claude/skills/rust-skills/rules/name-no-get-prefix.md)'s "make the common case concise *and* legible")
- **Site:** `src/render.rs:100` (`describe`), `src/render.rs:92` (`refusal`), `src/ui/statusbar.rs:152` (`against`)
- **What:** three single-word free functions whose names name neither their
  input nor their output:
  - `fn describe(reason: StopReason) -> &'static str` — imported bare into
    `src/app/session/mod.rs:13` and called as `describe(*reason)`. In a file
    full of `Action`s, `Event`s and `Recovery` rungs, "describe" could describe
    any of them.
  - `fn refusal(reason: RefusalReason) -> &'static str` — same shape, same
    import, and the noun collides conceptually with `Action::Refused` and with
    `pcap.rs`'s unrelated `struct Refusal`.
  - `fn against(value: u32, limit: Option<u32>) -> String` — renders `3/10` or
    `3/—`. It reads acceptably at its one call site
    (`against(view.progress.refreshes, view.limits.max_refreshes)`) and not at
    all anywhere else.
- **Why it matters here:** `src/render.rs` already has the right convention in
  the same 120 lines — `kind_label`, `status_label`, `merchant_label`,
  `status_summary`, `format_item` — so these two are the exceptions rather than
  the pattern, and they are the two that leave the module (`describe` and
  `refusal` are the only `render` items `session/mod.rs` imports besides the
  `*_label`/`render_*` pair). This is the weakest finding in the report; skip it
  if it costs more than the rename.
- **Fix:** `describe` → `stop_reason_label`, `refusal` → `refusal_label`,
  `against` → `value_over_limit` (or inline it). Three call sites total.
- **Effort:** trivial

## Clean areas

**Compiler-guaranteed, verified not suppressed:**
- `name-types-camel` / `name-variants-camel` / `name-funcs-snake` /
  `name-consts-screaming` — no `#[allow(non_snake_case)]`,
  `#[allow(non_camel_case_types)]` or `#[allow(non_upper_case_globals)]` exists
  anywhere in `src/`, `examples/` or `build.rs`, and `cargo clippy --all-targets`
  emits nothing. I enumerated every `struct`/`enum`/`trait`/`type` declaration
  and every `const`/`static` name anyway as a cross-check: all types are
  `UpperCamelCase`, all constants `SCREAMING_SNAKE_CASE` (the only lowercase
  hits were `pcap_stat` and `timeval` *inside doc comments*, referring to the C
  types being mirrored). The one thing worth saying out loud is that the FFI
  block in `src/capture/pcap.rs` mirrors `pcap.h` in *Rust* casing
  (`PcapIf`, `PcapPktHdr`, `BpfProgram`, `PcapStat`, `PcapT`) with the C names
  kept only in the `sym!(b"pcap_findalldevs\0")` string literals — which is
  exactly right, and needs no `allow`.

**Genuinely audited and correct:**
- `name-acronym-word` — the protocol/Win32 acronym minefield is clean. `BpfProgram`
  not `BPFProgram`; `Wpcap` not `WPCAP`; `WsError` (the `tungstenite::Error`
  alias in `src/uplink/websocket.rs:12`) not `WSError`. There is no `IPv4`,
  `TCP`, `TLS`, `UI`, `DPI` or `DACL` in any type name — the crate consistently
  reaches for role names (`FlowKey`, `Segment`, `LinkStrip`, `PacketSource`)
  instead. In `snake_case` the acronyms are correctly lowercased throughout:
  `parse_segment`, `ip_bytes`, `ensure_dpi_awareness`, `dacl_is_protected`,
  `is_loopback_ws_host`, `npcap_admin_only`, `ethernet_payload_offset`.
- `name-as-free` — the crate's only `as_` methods return borrows for free:
  `BudgetedChunk::as_slice` (`src/stream.rs:324`) is `&self.bytes`, and the
  `Deref` impls beside it forward to it. No allocating `as_` anywhere.
- `name-to-expensive` — both `to_` sites compute a new owned value from a cheap
  receiver, which is what the prefix promises: `plan::to_screen`
  (`src/actuator/plan.rs:130`, the design→screen transform) and
  `Target::to_client` (`src/actuator/win.rs:604`, `Copy` receiver, screen→client).
  Neither is a disguised borrow. Do not "fix" these to `as_`.
- `name-into-ownership` — both `into_` methods really consume `self`:
  `BudgetedChunk::into_parts` (`src/stream.rs:358`, destructures into
  `(Vec<u8>, PayloadLease)` — the textbook `into_parts`) and
  `InitialBurst::into_ordered` (`src/stream.rs:492`).
- `name-no-get-prefix` — **zero** `get_`-prefixed getters in the entire crate.
  `Controller` alone exposes nine correctly-bare accessors (`status`,
  `progress`, `haul`, `limits`, `filter`, `checklist`, `refresh_meta`,
  `last_snapshot`, `gold_balance`), and the getter/setter pairs follow the rule
  exactly: `ActuatorHandle::timings()` / `set_timings()`
  (`src/actuator/mod.rs:94`/`102`).
- `name-is-has-bool`, the correct majority — `is_enabled`, `is_unrestricted`,
  `is_sold_out`, `is_inert`, `is_at_limit`, `is_refusal`, `is_plausible…`'s
  neighbours `is_commented_assignment` and `is_header_of`, `is_loopback_ws_host`,
  `dacl_is_protected`. The rule's documented exceptions are also used correctly
  and must not be "fixed": `Filter::matches(item)` and
  `SubstatReq::satisfied_by(item)` are argument-taking verb phrases (the rule's
  own `str::starts_with` carve-out); `InitialBurst::would_exceed` and
  `syn_starts_new_incarnation` are verb phrases that read as the question they
  ask. Likewise the action-returning-status `bool`s — `try_retag`,
  `try_retag_pending`, `try_enqueue`, `PressureResync::request`, `reserve_new`,
  `buffer_future`, `drain`, `strip_table`, `drain_until`, `deliver_command`,
  `take_capture_loss` — are commands reporting an outcome, the
  `HashSet::insert -> bool` shape, not predicates; the prefix rule does not
  apply to them.
- `name-lifetime-short` — the only lifetime names in the crate are `'a` (8),
  `'de` (4), `'_` (9) and `'static`. Elision is used wherever it works.
- `name-type-param-single` — every generic parameter is a single uppercase
  letter, and each is the conventional one: `T` for the guarded/serialized
  value (`blocking<T>`, `lock<T>`, `section_table<T>`, `limit_row<T>`,
  `optional_value<T>`, `lock_ignoring_poison<T>`), `R` for a closure return
  (`content_inset<R>`), `C`/`F`/`S` for connector/future/stream
  (`run_with_connector<C, F, S>`, `pump<S>`).
- `name-crate-no-rs` — `name = "arkyve-refresh-shop"` (`Cargo.toml:2`): no `-rs`,
  no `-rust`, no `rust-` prefix. It names the product, not the language.

## Not applicable

- `name-iter-convention`, `name-iter-method`, `name-iter-type-match` — the crate
  defines no iterator method and no iterator type. Grepping `impl Iterator`,
  `IntoIterator`, `Iterator for` and `fn iter` across `src/` and `examples/`
  returns nothing: collections are exposed as slices (`Controller::checklist()
  -> &[u32]`) or as owned `Vec`s (`EventLog::entries()`), and every iteration
  in the crate is over a std collection's own `iter()`. Nothing to name, so
  nothing to get wrong.
- `name-crate-no-rs` is technically moot beyond the check above: `publish = false`,
  so the name never reaches crates.io. It is honoured anyway.
