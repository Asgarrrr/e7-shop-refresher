# 14 — Serde (`serde-`)

**Category priority:** MEDIUM
**Rules audited:** 8 · **Files read:** 22 · **Findings:** 5 (P0 0 / P1 1 / P2 4 / P3 0)

## Verdict

The **config surface is the best-audited serde code in this crate** and needs almost
nothing: every section carries `#[serde(default, deny_unknown_fields)]`, every typo
class has a named regression test, the shipped `config.example.toml` is parsed *and*
validated by a test so the schema cannot drift from the example, and the
`deny_unknown_fields`-vs-field-removal tension was paid off properly in `97e8807`
(`strip_retired_keys`) rather than papered over. The **wire surface is the weak one**:
`src/uplink/protocol.rs` is 84 lines that decide whether the product works at all,
and the one string that matters — the `"shop"` discriminator — is pinned by no
fixture, while `#[serde(other)]` guarantees that getting it wrong produces silence
rather than an error. Worst offender: `src/uplink/protocol.rs`. Highest-value fix:
**serde-001** — three JSON fixtures (`{"type":"ack"}`, `{"type":"shop",…}`, and the
unpinned `ShopItem` fields) turn a silent product-death into a failing test.

Secondary theme, across three findings: validation of user-authored values is
*post-hoc* (`Config::validate`, a method on the root) rather than *at deserialize
time* (`serde(try_from)`), which is why the same invariant is re-derived in the
config loader, the timing editor and the plan engine — and why one of the three
(`min_grade`) simply has no check at all.

## Findings

### serde-001 — the load-bearing wire discriminator is pinned by no fixture, and `serde(other)` makes a mismatch silent

- **Severity:** P1
- **Rule:** [`serde-enum-representation`](../../.claude/skills/rust-skills/rules/serde-enum-representation.md)
- **Site:** `src/uplink/protocol.rs:10-23` (+ tests at `src/uplink/protocol.rs:38-84`; unpinned field names at `src/domain/shop.rs:7-19`, `68-100`, `131-140`)
- **What:** `ServerMessage` is internally tagged and explicitly so — good — but the
  tagging is combined with a catch-all:

  ```rust
  #[serde(tag = "type", rename_all = "snake_case")]
  pub enum ServerMessage {
      Ack,
      Shop(ShopSnapshot),
      Purchase(PurchaseNotice),
      #[serde(other)]
      Unknown,
  }
  ```

  `rename_all = "snake_case"` means the wire tags are `"ack"`, `"shop"`,
  `"purchase"`. **Only `"purchase"` is exercised by a JSON fixture** (lines 48, 63,
  72). Nothing in the repo ever deserializes `{"type":"shop",…}` or
  `{"type":"ack"}`: all 20-odd session tests build `ServerMessage::Shop(…)` as a Rust
  value (`src/app/session/tests.rs:670` and on), bypassing serde entirely. Same gap
  one level down — `src/domain/shop.rs`'s fixtures pin `refresh`,
  `crystal_balance`, `cost`, `slots`, `id`, `limit`, `remaining`, `total`,
  `substats`, `name`, `value`, but **not** `merchant`, `slot`, `kind` (nor any of
  `ItemKind`'s three snake_case spellings), `price`, `grade`, `set`.
- **Why it matters here:** renaming the variant `Shop` (or the field `price`, or
  `ItemKind::Equipment`) is a one-keystroke refactor that no compiler and no test
  objects to, and `#[serde(other)]` converts the resulting mismatch into
  `ServerMessage::Unknown`, which `src/app/session/mod.rs:317` drops as a no-op. The
  observable result is an app that connects, logs "server link established", and
  never shows a shop — with a green CI. The client is an exe in players' hands and
  the server deploys independently, so this contract is also the only thing keeping
  the two halves compatible, and it exists nowhere as an artefact: no schema, no doc,
  no fixture.
- **Fix:** add fixtures to `protocol.rs`'s test module that pin the tag strings and
  the shop payload end to end — one per variant, plus one `ShopItem` with every field
  populated:

  ```rust
  #[test]
  fn the_shop_tag_and_every_item_field_are_the_wire_contract() {
      let ServerMessage::Shop(shop) = parse(
          r#"{"type":"shop","merchant":"m","slots":[{"id":1,"slot":2,
             "kind":"equipment","name":"n","price":100,"grade":4,"set":"set_speed"}]}"#
      ) else { panic!() };
      assert_eq!(shop.merchant.as_deref(), Some("m"));
      let item = &shop.slots[0];
      assert_eq!((item.slot, item.kind, item.grade), (2, ItemKind::Equipment, Some(4)));
      assert_eq!(item.price, Some(100));
      assert_eq!(item.set.as_deref(), Some("set_speed"));
  }
  #[test]
  fn the_ack_tag_parses_as_ack_not_unknown() {
      assert!(matches!(parse(r#"{"type":"ack"}"#), ServerMessage::Ack));
  }
  ```

  While there, state the representation's constraint on the enum's doc comment: an
  internally tagged enum cannot carry a newtype variant wrapping a primitive or a
  `Vec` — it works today only because `ShopSnapshot` and `PurchaseNotice` both
  deserialize from maps. A future `Error(String)` variant compiles and fails at
  *runtime*, swallowed by `forward`'s `debug!`.
- **Effort:** small

### serde-002 — `ServerMessage::Unknown` is discarded with no log line, so a tag mismatch is indistinguishable from a mute server

- **Severity:** P2
- **Rule:** [`serde-enum-representation`](../../.claude/skills/rust-skills/rules/serde-enum-representation.md)
- **Site:** `src/uplink/protocol.rs:20-22`, consumed at `src/app/session/mod.rs:316-317`; contrast `src/uplink/websocket.rs:215-222`
- **What:** the two failure modes of the inbound path are handled asymmetrically. A
  payload that fails to deserialize is logged — `forward` does
  `Err(err) => debug!(error = %err, "unrecognized server message, ignored")`, and the
  default filter (`arkyve_refresh_shop=debug`, `main.rs:92`) captures it in the log
  file. A payload with an *unrecognized tag* deserializes **successfully** into
  `Unknown` and then hits `ServerMessage::Ack | ServerMessage::Unknown => {}` — no
  log, no counter, nothing.
- **Why it matters here:** `heartbeat` (`src/app/session/mod.rs:190-212`) exists
  precisely to tell apart "the three ways *it stopped refreshing* happens — capture
  blind, server mute, actuator stuck". `serde(other)` silently adds a fourth: server
  talking, client not understanding. It presents as `since_last_shop_s = None`, i.e.
  identical to "server mute", in a windowed build whose only diagnostic channel is
  that log file and whose troubleshooting section (README) tells the player to send
  it. This is the runtime half of serde-001: together they mean a protocol skew is
  both untestable and undiagnosable.
- **Fix:** make the catch-all observable. Either give `Unknown` the tag it absorbed
  (`#[serde(other)] Unknown` cannot capture it, so instead deserialize the tag
  separately, or) — simpler and enough — log once per session from `on_message`:

  ```rust
  ServerMessage::Unknown => {
      // Not silent: an unknown tag means the server speaks a dialect this
      // build does not, which otherwise reads exactly like a mute server.
      tracing::warn!("server sent a message type this build does not understand");
  }
  ServerMessage::Ack => {}
  ```

  and add the count to `heartbeat`'s fields so the log tells the fourth case apart.
- **Effort:** trivial

### serde-003 — `Filter` writes four no-op keys into the player's `config.toml` on every Apply, unlike its `Timings` sibling

- **Severity:** P2
- **Rule:** [`serde-skip-empty`](../../.claude/skills/rust-skills/rules/serde-skip-empty.md)
- **Site:** `src/domain/filter.rs:19-39` (no `skip_serializing_if`); written at `src/config/persist.rs:344`, `357-360`; contrast `src/actuator/plan.rs:228-255`
- **What:** `Filter` derives `Serialize` with no `skip_serializing_if` on its four
  never-`None` fields, and `persist::save` does whole-section replacement. Measured
  by replaying `write_sections` against the locked `toml_edit 0.25.12` with the
  crate's exact types — the first Apply after the player types one item name writes:

  ```toml
  [filter]
  kinds = []
  names = ["ticketrare_name"]
  sets = []
  required_substats = []
  include_sold_out = false
  ```

  Four of those five lines are inert. (`Option` fields are correctly absent —
  `toml_edit` omits `None` — which is why `min_substats`/`max_price`/`min_grade`
  don't appear and why `none_limit_omits_its_key` passes.)
- **Why it matters here:** `config/persist.rs`'s module doc says the point is that
  "every other section is left exactly as the player wrote it", and the sibling
  `Timings` was given `#[serde(skip_serializing_if = "DelayRange::is_inert")]` on all
  eight fields for exactly this reason, with a test whose comment states it:
  *"Without the skips, the first Apply after touching one knob wrote all eight ranges
  — seven of them no-ops — into a file this module exists to leave alone"*
  (`src/config/persist.rs:548-577`). `Filter` is the section the player edits most and
  it never got the same treatment: the file the README tells them to inspect gains
  four lines of noise that say nothing, and `[filter]`'s hand-written commented
  examples are dropped in the same pass.
- **Fix:** mirror the `Timings` pattern; the container `#[serde(default)]` already on
  `Filter` makes every omission round-trip.

  ```rust
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub kinds: Vec<ItemKind>,
  // …same for `sets` and `required_substats` (`names` too: an empty hunt is
  // refused by the arming check, so `names = []` is never meaningful either)
  #[serde(default, skip_serializing_if = "std::ops::Not::not")]
  pub include_sold_out: bool,
  ```

  Extend `inert_timing_ranges_are_not_written` with a `Filter` twin so it stays
  fixed. Separately worth noting while in there: `required_substats` renders as an
  inline `[{ name = "speed", min = 8.0 }]`, not the `[[filter.required_substats]]`
  array-of-tables the example documents — `inline_ranges`
  (`src/config/persist.rs:392-399`) shows the crate already cares about matching the
  documented hand-written style, and nothing today covers persisting a filter with a
  substat requirement.
- **Effort:** trivial

### serde-004 — `min_grade` and `min_substats` accept out-of-domain values that silently match nothing, which is the exact failure `kinds` is checked for

- **Severity:** P2
- **Rule:** [`serde-try-from-validate`](../../.claude/skills/rust-skills/rules/serde-try-from-validate.md)
- **Site:** `src/domain/filter.rs:30`, `36`; the check that exists for the same failure mode is `src/config.rs:373-379`
- **What:** `min_grade: Option<u8>` is documented as "Inclusive minimum gear grade
  (2, 3, or 4)" and `config.example.toml:56` repeats the closed domain — but nothing
  rejects `min_grade = 44`. `matches` is deliberately fail-closed on grade
  (`src/domain/filter.rs:80-84`), so an out-of-domain floor drops every item forever;
  and `is_unrestricted` counts *any* grade floor as a real criterion
  (`src/domain/filter.rs:104-106`), so the loop **arms** on it. `min_substats` has the
  same shape.
- **Why it matters here:** `Config::validate` refuses an unrecognized `kinds` entry
  with an explicit rationale — *"`ItemKind` is wire-tolerant (`serde(other)` ->
  Unknown), which in a config file would let a typo silently match nothing: reject it
  here"* — and the retired-keys code repeats the principle: *"the player who set
  `capture.filter` to widen their capture would otherwise spend an evening wondering
  why"*. A typo'd `min_grade` produces the same outcome by a different route, and
  costs more than an evening: the loop keeps refreshing and keeps debiting crystals
  (`REFRESH_COST_CRYSTALS`) while never matching, bounded only by whatever `[limits]`
  the player happened to set — and `[limits]` defaults to none. `min_grade` is also
  config-file-only (the Setup tab exposes `min substats` and `max price` but no
  grade, `src/ui/editor/mod.rs:265-280`), so the config file is the *only* way to set
  it and the only place the typo can be caught.
- **Fix:** reject it while deserializing so the invalid value never exists, rather
  than adding a fourth clause to `Config::validate`:

  ```rust
  /// Inclusive gear-grade floor. `try_from` rather than a `Config::validate`
  /// clause: the domain is closed (the game ships grades 2..=4) and a floor
  /// outside it silently matches nothing.
  #[derive(Debug, Clone, Copy, PartialEq, Serialize)]
  #[serde(into = "u8")]
  pub struct GradeFloor(u8);

  impl TryFrom<u8> for GradeFloor {
      type Error = String;
      fn try_from(g: u8) -> Result<Self, String> {
          (2..=4).contains(&g).then_some(Self(g))
              .ok_or_else(|| format!("gear grade {g} does not exist (expected 2, 3 or 4)"))
      }
  }
  ```

  `toml` locates the span for free, so the error names the line. If a newtype is
  judged too heavy for one field, the minimum acceptable fix is a `min_grade` clause
  in `Config::validate` alongside the `kinds` one — but note that only covers the
  config path, whereas `try_from` also covers any future one. Decide `min_substats`
  deliberately either way: its domain is genuinely open (`min_substats_counts_duplicates`
  documents raw-length semantics), so a soft ceiling or nothing at all is defensible —
  just say which in the doc comment.
- **Effort:** small

### serde-005 — the timing invariants live on the root `Config`, not on `DelayRange`, so three separate places re-derive them and `persist::save` honours none

- **Severity:** P2
- **Rule:** [`serde-try-from-validate`](../../.claude/skills/rust-skills/rules/serde-try-from-validate.md)
- **Site:** `src/config.rs:39`, `409-422`; `src/actuator/plan.rs:184-215`; `src/ui/editor/timing_meter.rs:117-135`, `185-199`; bypassed at `src/config/persist.rs:51-65` via `src/ui/mod.rs:229-236`
- **What:** `DelayRange` accepts any `(min_ms, max_ms)` pair at deserialize time. The
  two invariants — `min_ms <= max_ms` and `max_ms <= MAX_TIMING_MS` — are enforced
  only by a loop in `Config::validate`, which walks `Timings::named_ranges()`. Every
  other construction path re-derives them or absorbs them:
  - `DelayRange::draw` reinterprets a reversed range as a fixed point and needs a
    `checked_add` guard *"[w]ithout this guard a `max_ms = u64::MAX` config would
    panic (`% 0`)"* (`plan.rs:199-214`);
  - `timing_meter` clamps `max_ms` to the 2 500 ms ruler and re-imposes
    `min_ms = min_ms.min(max_ms)` (`timing_meter.rs:118-123`), then uses
    `saturating_add` twice more with a comment justifying it;
  - `persist::save` serializes whatever `Timings` the GUI hands it, with **no
    `Config::validate` between the editor and the disk** (`ui/mod.rs:229-236`).
    Nothing today can produce an invalid one — the editor's clamps see to that, and
    `every_timing_preset_survives_validation` covers the presets — so this is latent,
    not live. But the guarantee is spread over three files and one of them is a GUI
    painter; it holds by coincidence of clamping, not by construction.

  Two comments have already gone stale on the strength of this split:
  `timing_meter.rs:131` (*"`[actuator.timings]` is not validated at load"*) and the
  test at `timing_meter.rs:221-232` (*"`[actuator.timings]` is not validated at load
  and the engine honours a full-range wait"*). `Config::validate` has validated it
  since `MAX_TIMING_MS` was added; what is still unvalidated is `DelayRange` *itself*,
  which is a different and much narrower claim than the comments make.
- **Why it matters here:** the failure this guards is the crate's own worst
  user-facing class — `src/ui/editor/mod.rs:228-233` documents a shipped bug of
  exactly this shape (a checkbox that wrote a `kinds = ["unknown"]` the next launch
  refused to load, *"and the load failure is fatal (error window, no main window).
  The only way out was hand-editing the very file the player is not expected to
  hand-edit"*). Any future writer of `Timings` — a preset import, a CLI flag, a
  `SetTimings` from a script — inherits that hazard, because the type carries no
  invariant of its own.
- **Fix:** make the range parse-don't-validate, so the invariant is structural:

  ```rust
  #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
  #[serde(default, deny_unknown_fields)]
  struct RawDelayRange { min_ms: u64, max_ms: u64 }

  #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
  #[serde(try_from = "RawDelayRange", into = "RawDelayRange")]
  pub struct DelayRange { min_ms: u64, max_ms: u64 }   // fields become private

  impl TryFrom<RawDelayRange> for DelayRange { /* the two checks from Config::validate */ }
  ```

  Then delete the loop in `Config::validate`, keep `draw`'s `checked_add` as
  defence-in-depth only, and drop or correct the two stale comments. Note the one
  real cost, so a later pass weighs it rather than discovers it: `MAX_TIMING_MS` lives
  in `config.rs` and `DelayRange` in `actuator/plan.rs`, so this moves the ceiling
  constant down into the plan module (an inversion of the current dependency
  direction) — and the carefully worded reversed-range error message
  (`config.rs:411-414`, which names the key *and* explains what the value would be
  read as) must be reproduced inside `TryFrom`; `toml` supplies the key's span, so
  little is lost, but it is not free. If that trade is rejected, the minimum is: fix
  the two stale comments, and add a test that a GUI-produced `Timings` reloads through
  `Config::load` (making the editor's clamp a *tested* part of the guarantee rather
  than an incidental one).
- **Effort:** medium

## Clean areas

**Config surface (`src/config.rs`, `src/config/persist.rs`, `config.example.toml`)**

- `serde-default-compat` — exemplary. `Config` and all five sub-structs carry
  container-level `#[serde(default)]`; every one derives or hand-writes `Default`. An
  old file survives a field addition, proven by `missing_filter_and_limits_sections_default`
  and `partial_sections_leave_other_fields_default`. The rule's caveat (container
  `default` hides field-name typos) is neutralised by pairing it with
  `deny_unknown_fields` on the same containers — the textbook combination.
- `serde-default-compat`, exception done right — `SubstatReq` (`filter.rs:46-52`)
  deliberately has *no* container default so `name` stays required, documented
  (*"a nameless requirement would silently match nothing"*) and tested
  (`required_substat_without_name_is_rejected`).
- `serde-deny-unknown-fields` — on all six config containers plus `DelayRange` and
  `SubstatReq`, and the argued position is the right one for this app: the config file
  is the only input a player writes by hand, the app owns the file, and a silently
  ignored key is worse than a refused one because it costs crystals
  (`dryrun` → real clicks; `max_refresh` → a limit that never fires). Each typo class
  has its own named test (`a_misspelled_forward_key_is_still_rejected`,
  `misspelled_actuator_key_is_rejected`, `misspelled_timings_key_is_rejected`,
  `misspelled_limit_key_is_rejected`, `unknown_actuator_backend_is_rejected`).
  **The cost is understood and paid, not ignored:** `deny_unknown_fields` makes field
  *removal* a breaking change for every file on disk, which the crate hit twice, and
  `97e8807` bought the way out — `strip_retired_keys` + `RETIRED_KEYS`
  (`persist.rs:114-117`) delete the retired keys at the same startup that warns about
  them, so the warning is one-time *in fact*. Do not "simplify" any of this: the two
  vestigial structs, their `retired_keys()` accessors, and the `tidy` text pass are all
  load-bearing and each carries the regression test that proves it.
- Schema-vs-example drift — none. `config.example.toml` covers every one of the 24
  keys `Config` accepts, `min_grade` included, and
  `bundled_example_config_parses_validates_and_is_restrictive` (`config.rs:956-1003`)
  parses it through the real type, validates it, asserts it carries a hunt criterion,
  asserts it re-plants no retired key, and round-trips it through `strip_retired_keys`
  on disk byte-for-byte. That test is the reason this category has no drift finding.
- `serde-rename-all` — correct throughout and needed exactly where used. TOML keys are
  snake_case and Rust fields are snake_case, so no container needs the attribute; the
  two config *enums* do (variants are PascalCase) and both have it:
  `ActuatorBackend` (`config.rs:137-149`) → `"input"`/`"message"`, matching the
  example, and `ItemKind` (`shop.rs:131-140`) → `"equipment"`/`"hero"`/`"token"`,
  matching it too. `ActuatorBackend` correctly has *no* `serde(other)` — it is
  config-only, so an unknown value must be an error, and is.
- `ItemKind::Unknown` never reaching disk — a genuine hazard, correctly closed:
  `Config::validate` refuses it on the way in, and the Setup tab deliberately omits its
  checkbox with a comment recording the bug that box used to cause
  (`ui/editor/mod.rs:228-233`). Leave the box out.
- `toml_edit` round-tripping — `section_table` + `set_table` preserve the replaced
  table's decor so above-header comments survive; `inline_ranges` re-renders each
  `DelayRange` as `{ min_ms = .., max_ms = .. }` to match the hand-written style. The
  constraint this puts on serde attributes (whole-section replacement, so every
  serialized field lands in the file) is understood — it is exactly what
  `skip_serializing_if` on `Timings` exists for, and exactly what serde-003 asks for
  on `Filter`.

**Wire surface (`src/uplink/`, `src/domain/shop.rs`)**

- `serde-enum-representation`, the choice itself — internally tagged is *stated*
  (`tag = "type"`), not defaulted into, and it is the right one: a `{"type":…}`
  discriminator is what a JSON server emits, and every variant's payload is a map so
  the representation's real constraint is satisfied. The findings above are about
  pinning and observability, not about the choice.
- `serde-deny-unknown-fields`, correctly **absent** — no wire struct has it, and that
  is right. The client ships as an exe to players who may never upgrade; the server
  deploys continuously. The upgradeable side must be the strict one, so leniency
  belongs here: unknown keys ignored, unknown tags folded to `Unknown`, partial
  side-objects degraded to `None`. `filter.rs:16-18` states the asymmetry explicitly
  (*"Unlike the wire models, unknown keys are rejected"*). **Do not add
  `deny_unknown_fields` to `ShopSnapshot`, `ShopItem`, `PurchaseNotice`, `RefreshMeta`,
  `SubStat` or `PurchaseLimit`** — it would turn every server-side field addition into
  a total outage for every installed exe.
- `serde-custom-with` — the two `deserialize_with` helpers are the best serde code in
  the crate. `object_or_none` and `lenient_elements` (`shop.rs:43-66`) are generic,
  reusable, documented with the reason a bare `?` cannot do the job (*"would abort the
  surrounding message mid-stream"*), and each has its own degradation tests
  (`refresh_partial_object_degrades_to_none`, `refresh_mistyped_degrades_to_none`,
  `bad_substat_entry_is_dropped_not_fatal`, `mistyped_substats_degrade_to_empty`).
  They are deliberately one-directional, so a `with` module would be wrong here.
- No hand-written `Serialize`/`Deserialize` impls anywhere in the crate — verified by
  grep. Nothing that should have been a `with` module was open-coded instead.
- `serde-default-compat` on the wire — `PurchaseNotice`'s both fields default
  (`{"type":"purchase"}` alone parses, tested), `ShopSnapshot`/`ShopItem` default every
  field. `RefreshMeta`, `PurchaseLimit` and `SubStat` deliberately do *not* default
  their required fields, because each is reached only through `object_or_none` /
  `lenient_elements`, which turns a partial object into `None`/dropped rather than into
  a failed snapshot — a better outcome than a defaulted-to-zero crystal balance.
- Field-level `#[serde(default)]` on all nine `ShopItem` fields could collapse to one
  container-level attribute (it already derives `Default`). Judged a wash, not filed:
  per-field is more explicit on a wire model where each field's tolerance is a decision.
  Noted only so nobody files it as a finding later.
- The `0` sentinel on `ShopItem::id` / `slot` / `PurchaseNotice::item` — not filed.
  Every interpretation site is guarded and documented: `catalog_id()` is the single
  home for the comparison (*"do not re-derive"*), `effective_slot` clamps away from the
  sentinel, `dedup::fingerprint` returns `None` when any id is 0, and
  `on_purchase` guards `if item != 0`. `Option<u32>` would be more serde-idiomatic but
  buys nothing here.

**Milliseconds as `u64` — considered and deliberately not `serde(with)`**

`serde-custom-with`'s canonical improvement (a `Duration` field behind a
`with = "duration_ms"` module instead of a bare `*_ms: u64`) was assessed and is
**not** filed as a finding. `u64` milliseconds is the crate's pervasive time unit,
not a serde workaround: `EventLog::now_ms` is the session clock, `Limits::max_duration_ms`
is compared against it directly, `DelayRange` feeds `saturating_add` against `u64`
baselines and a `%` modulus in `draw`, and the `*_ms` key spellings are the documented
public config schema (`config.example.toml:99-107`). The one field pair with purely
`Duration`-shaped consumers, `ReconnectConfig::{initial_ms, max_ms}`, is read through
`reconnect_initial()`/`reconnect_max()`, which *normalise* (floor at `RECONNECT_FLOOR`,
order the two) as well as convert — work a `with` module cannot do, so the accessors
would survive the change and it would only add `rename` noise. Leave as is.

## Not applicable

- `serde-flatten` — unused, and correctly so. There is no repeated field group to
  inline (`Timings`' eight `DelayRange`s are eight distinct keys, not a shared block),
  and no catch-all is wanted: `flatten` is incompatible with `deny_unknown_fields`,
  which is the deliberate policy on every config struct. Introducing `flatten`
  anywhere in `config.rs` would silently disable the typo detection that finding
  serde-004's siblings rely on.
- Local persisted state / schema versioning — there is none to version. The journal
  (`src/journal.rs`) is an in-memory `VecDeque<LogLine>` with no serde at all; its
  durable mirror is `tracing` text lines. Nothing under `%LOCALAPPDATA%` is
  serde-serialized: `crash.log` is `format!`-built text (`src/crash.rs:54-64`) and
  `logs\*.log` is `tracing-subscriber` output. The only serde-written file in the
  product is `%APPDATA%\arkyve-refresh-shop\config.toml`, audited above as the config
  surface; it carries no schema-version key and needs none — forward compatibility
  comes from container `#[serde(default)]` and backward compatibility from the
  `RETIRED_KEYS` retirement path.
- `api-serde-optional` — a sibling category's rule, and irrelevant here regardless:
  this crate is `publish = false` (`Cargo.toml:8`), a binary and not a library, so
  there is no downstream consumer to spare the `serde` dependency and no reason to
  feature-gate the derives.
- Outbound wire serialization — the uplink sends `Message::Binary(bytes)`
  (`websocket.rs:187`), raw reassembled TCP payload. There is no client→server serde
  surface at all, so `serde-skip-empty` and `serde-rename-all` have nothing to say
  about the outbound direction.

---

*Method note, for auditability:* every `.rs` file with a serde surface was read in
full — `config.rs`, `config/persist.rs`, `domain/filter.rs`, `domain/shop.rs`,
`domain/control/mod.rs`, `domain/control/dedup.rs`, `uplink/{mod,protocol,websocket}.rs`,
`journal.rs`, `actuator/plan.rs`, `main.rs`, `error.rs`, `crash.rs`, plus the relevant
spans of `ui/mod.rs`, `ui/editor/mod.rs`, `ui/editor/timing_meter.rs` and
`app/session/mod.rs` — alongside `Cargo.toml`, `config.example.toml`, the gitignored
local `config.toml`, and the README's config section. An exhaustive grep for
`serde|Serialize|Deserialize` across `src/`, `examples/` and `build.rs` returns zero
hits outside those files, so the remaining modules (capture, stream, actuator/win,
migrate, watch, render, the rest of ui) carry no serde surface for this category to
judge. `cargo clippy --all-targets --all-features` is clean, so none of the five
findings is already machine-flagged. serde-003's exact output was measured by
replaying `persist::write_sections` against `toml_edit 0.25.12+spec-1.1.0` (the locked
version) with the crate's own type definitions, not inferred.

*One observation outside the audit's scope, recorded because it was noticed:* the
repo-root `config.toml` is gitignored (developer-local, not shipped) and has drifted
from the example — it still describes `capture.filter` as a *"manual WinDivert filter
override"* and claims `"input" (default)` for `actuator.backend`, which has been
`Message` since `ActuatorBackend`'s `#[default]` moved. It also sets three of the four
retired keys, so it triggers the one-time strip on the next dev launch. Nothing to fix
in the crate; noted in case the drift is mistaken for a schema finding by a later pass.
