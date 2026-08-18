# 18 — Collections (`coll-`)

**Category priority:** MEDIUM
**Rules audited:** 4 · **Files read:** 39 · **Findings:** 2 (P0 0 / P1 0 / P2 0 / P3 2)

## Verdict

This category is clean. Every collection in the crate is the one its observed
access pattern asks for: `BTreeMap` where the code calls `first_key_value` /
`pop_first` (`src/stream.rs:674`), `VecDeque` where it calls `push_back` /
`pop_front` (`src/journal.rs:29`, `src/stream.rs:501`), `HashMap` where only
keyed lookup happens (`src/stream.rs:539`), and fixed arrays-of-tuples where a
map would hold two to eight entries (`src/render.rs:15`,
`src/config/persist.rs:114`, `src/actuator/plan.rs:264`) — which is literally the
bottom row of `coll-map-choice`'s decision table. There is no `HashSet`,
`BTreeSet`, `BinaryHeap`, `LinkedList` or `IndexMap` anywhere in the crate, and
none is missing: every `.contains(` / `.iter().any(` in production code runs over
**≤ 8 elements** (6 shop slots, 2 haul headliners, 3 item kinds, 2 retired-key
tables, 2 halt sources), which `coll-set-membership` explicitly rules fine.
`cargo clippy --all-targets` is silent. The two findings below are P3 cliff
notes, not defects: the linear membership scans are correct at today's `n` and
would only become quadratic if `ShopSnapshot::slots` or `Controller::bought` grew
past the handful of entries the game produces — and neither is capped, unlike
every other wire-fed collection in this crate. Highest-value action in this
category: nothing. Do not "optimise" the sites listed under **Clean areas** —
three of them would be actively broken by the obvious change.

## Findings

### coll-001 — Checklist membership is O(n·m) against an uncapped wire-sized slot list

- **Severity:** P3
- **Rule:** [`coll-set-membership`](../../.claude/skills/rust-skills/rules/coll-set-membership.md)
- **Site:** `src/ui/view.rs:70` (render path, 4 Hz) · also
  `src/domain/control/watchdog.rs:140`, `src/domain/control/mod.rs:535`
- **What:** Three nested scans pair the slot list against a `Vec<u32>` membership
  test. The render one is the hot one:

  ```rust
  // src/ui/view.rs:58-74 — once per slot, every frame
  wanted: item.catalog_id().is_some_and(|id| checklist.contains(&id)),
  ```

  `checklist: Vec<u32>` (`src/domain/control/mod.rs:254`) is rebuilt per snapshot
  from the matched slots, so `m == n` in the worst case and the loop is O(n·m).
  `src/domain/control/watchdog.rs:130-146` has the same shape over
  `snapshot.slots` × `checklist`, and `src/domain/control/mod.rs:525-550` does
  `self.bought.contains(&id)` once per slot inside `plan_targets`.
- **Why it matters here:** **n = 6.** The Secret Shop has six slots; every
  fixture in `src/domain/control/tests.rs` and `examples/ui_preview.rs` builds
  exactly six, and `checklist` is 1–3 in every test. 6 × 6 = 36 `u32` compares
  per frame is free, and `Vec` beats `HashSet` at that size — this is the "tiny
  set (≤ 8 items)" row of the rule, not a violation of it. The cliff is that
  `slots` is `Vec<ShopItem>` deserialized straight off the wire
  (`src/domain/shop.rs:12`) with **no length cap**, and the crate already treats
  an oversized shop as a real input shape: `ShopItem::effective_slot` clamps for
  it (`src/domain/shop.rs:122-128`) and `buy_target_slot_clamps_oversized_position`
  (`src/domain/control/tests.rs:1274`) exercises **300 slots**. At 300 the render
  scan is 90 000 compares per frame — still ~50 µs, still fine. It stops being
  fine somewhere around 10⁴ slots, where 10⁸ compares per frame at 4 Hz would
  wedge the window and, in `recovery_buy_targets`, hold the controller mutex
  across it. That bound does not exist today, which is the only reason this is
  worth recording.
- **Fix:** Leave the `Vec`s alone — they are correct at every size the game
  produces. The real fix is a length cap on `ShopSnapshot::slots` at
  deserialization, which belongs to the input-validation category, not this one;
  cross-reference it there. Only if that cap is refused should `view_state` build
  the set once per frame instead of per row:
  ```rust
  let wanted: HashSet<u32> = controller.checklist().iter().copied().collect();
  // …then `wanted.contains(&id)` per row — O(n + m).
  ```
  That trade is a pessimisation at n = 6, so it must not land without the
  unbounded-input premise.
- **Effort:** trivial (as written: none — record and move on)

### coll-002 — `bought` and `Haul::named` grow monotonically per roll with no cap, and `bought` is scanned linearly on every echo

- **Severity:** P3
- **Rule:** [`coll-set-membership`](../../.claude/skills/rust-skills/rules/coll-set-membership.md)
- **Site:** `src/domain/control/mod.rs:568` (+ `:266` declaration, `:197` and
  `:209-217` for the `Haul` twin)
- **What:** `bought: Vec<u32>` is a pure dedup set — it is never iterated in
  order, only membership-tested — yet every purchase echo pays an O(n) scan
  before appending:

  ```rust
  // src/domain/control/mod.rs:568-569
  if item != 0 && !self.bought.contains(&item) {
      self.bought.push(item);
  ```

  N distinct echoes within one roll therefore cost O(N²). `Haul::named`
  (`BTreeMap<String, u32>`) is fed from the same path (`:220-228`) and grows the
  same way; `Haul::others` (`:209-217`) then walks all of it once per frame via
  `haul_tally` (`src/render.rs:24-28`).
- **Why it matters here:** **n is a handful.** Both collections are scoped to one
  roll — `bought` is cleared whenever the snapshot fingerprint changes
  (`:468-471`), `Haul` on `Start` (`:406`) — and a roll holds six items, so
  `bought.len() ≤ 6` and `named.len()` is a few dozen across a long run. At that
  size `Vec::contains` is the right call and `HashSet` would be slower. What is
  notable is that neither has a cap while **every other wire-fed collection in
  this crate does**: `MAX_STREAMS = 64` and `MAX_PENDING_BYTES`
  (`src/stream.rs:31,431`), `INITIAL_ANCHOR_MAX_BYTES` / `_SEGMENTS`
  (`src/stream.rs:435-436`), `JOURNAL_CAP = 500` (`src/journal.rs:21`). A server
  echoing distinct ids without a matching snapshot rotation grows both without
  bound (the `Purchase` handler runs whatever the status — see the comment at
  `:562-567`), and the `contains` turns that into quadratic CPU on top of the
  memory. `Progress` counters saturate; these two do not.
- **Fix:** The collection type is not the defect, so do not swap it on this
  finding alone. If the bound is added (a `MAX_BOUGHT_PER_ROLL` / `MAX_HAUL_NAMES`
  in the same spirit as `MAX_STREAMS`), keep `Vec` and `BTreeMap`. If instead the
  decision is that these may grow with the wire, then `bought` should become
  `HashSet<u32>` — nothing depends on its order, `PartialEq` on `Haul` is
  unaffected, and `insert()` replaces the contains-then-push pair in one call:
  ```rust
  if item != 0 && self.bought.insert(item) { /* …record the haul… */ }
  ```
- **Effort:** trivial

## Clean areas

**`coll-seq-choice` — every sequence type is the right one, and three of them
would break if "fixed".**

- `src/journal.rs:29` — `VecDeque<LogLine>` with `push_back` + `pop_front` under
  `JOURNAL_CAP = 500` (`:82-89`). Textbook bounded ring; the `Vec` version would
  be O(n) per line.
- `src/journal.rs:100-107` + `src/ui/journal.rs:119-121` — the reader copies the
  ring into a `Vec<LogLine>` and the body does `&journal[rows]`. That is not a
  redundant conversion: `egui`'s `show_rows` needs an **indexable contiguous
  slice**, which `VecDeque` cannot provide. Ring where it is a ring, `Vec` where
  it is randomly sliced — exactly right, and the `generation` counter
  (`src/ui/mod.rs:142-146`) keeps the clone off the repaint path.
- `src/stream.rs:497-533` (`InitialBurst::into_ordered`) — builds
  `HashMap<FlowKey, Vec<_>>`, sorts each flow by sequence, converts to
  `VecDeque` (`:501`) purely to `pop_front` one segment per recorded slot
  (`:524-532`). `VecDeque` is introduced at precisely the point pop-front
  behaviour begins, and `Vec::from`/`into` makes the switch free.
- `src/stream.rs:739` — `payload.bytes.drain(..already)`. **This looks like the
  `coll-seq-choice` bad example and is not one.** It is a one-shot prefix trim on
  an overlapping retransmission, not a repeated pop-front: `absorb` runs it at
  most once per segment, only when `already != 0`, so there is no O(n²) loop. And
  `VecDeque` would actively break the design — `BudgetedChunk` exposes
  `as_slice()`/`Deref<Target = [u8]>` (`:324,378-384`), the outbound path hands
  the buffer to `Message::Binary(bytes.into())`
  (`src/uplink/websocket.rs:187`), and the byte-budget lease records
  `Vec::capacity()` (`:127-142`), all three of which require a single contiguous
  allocation. The cost is one memmove of ≤ one coalesced frame on a rare path;
  if it ever mattered the fix is a start-offset field, not a different sequence
  type.
- `src/actuator/plan.rs:419` + `src/actuator/mod.rs:312` — `Job.steps: Vec<TimedStep>`
  consumed by `for step in &job.steps`, never from the front. `Vec` correct.
- `src/actuator/plan.rs:522-524` — `rows` filtered, `sort_unstable`, `dedup` on
  ≤ 6 `u8`. Cheaper than any set; the sort is load-bearing (scroll grouping).
- `src/capture/pcap.rs:622` — `self.threads.drain(..)` drains the whole `Vec`
  once at teardown; not a queue.
- `src/actuator/win.rs:779-786` — the fake driver scripts outcomes in
  `VecDeque`s and `pop_front`s them. FIFO behaviour, FIFO type.
- `src/domain/control/mod.rs:591` — `checklist.swap_remove(position)`: order is
  explicitly irrelevant there, and `swap_remove` is the O(1) choice over
  `remove`.

**`coll-map-choice` — chosen by access pattern in every case.**

- `src/stream.rs:674` — `pending: BTreeMap<i64, BudgetedChunk>`. `drain`
  (`:777-789`) needs `first_key_value` + `pop_first`; a `HashMap` could not
  answer "is the lowest buffered offset contiguous yet".
- `src/stream.rs:539` — `streams: HashMap<FlowKey, HalfStream>`. Only keyed
  `get`/`entry`/`remove` and an order-independent `min_by_key`; iteration order
  is never observed. `FlowKey` derives `Hash` (`src/capture/mod.rs:56`).
- `src/stream.rs:496-533` — the burst `HashMap`'s nondeterministic iteration
  order is deliberately made unobservable: the pre-recorded `slots: Vec<FlowKey>`
  replays the map. No `IndexMap` need, and none of the "iterate then sort"
  antipattern — the sort at `:519` is *inside* each value, by TCP sequence.
- `src/render.rs:15` `[(&str, &str); 2]`, `src/config/persist.rs:114`
  `&[(&str, &[&str])]`, `src/actuator/plan.rs:264` `[(&'static str, DelayRange); 8]`,
  `src/actuator/plan.rs:333` `[TimingPreset; 3]`, `src/ui/view.rs:34`
  `[(&'static str, u32); 2]`, `src/ui/editor/mod.rs:597` `[u64; 4]` — six tiny
  maps/lists as fixed arrays of tuples instead of a `HashMap`. This is the rule's
  own bottom-row recommendation, it keeps output order deterministic without
  taking the `indexmap` dependency (confirmed absent from `Cargo.toml`), and
  `named_ranges`' exhaustive destructuring (`:265-274`) turns a forgotten entry
  into a compile error. Exemplary; leave alone.
- `src/domain/control/mod.rs:197` — `Haul::named: BTreeMap<String, u32>`. Strictly
  by the rule this wants `HashMap` (neither `count` nor `others` needs order),
  **but do not swap it**: at n ≈ 5 a B-tree of short `String`s beats SipHash-1-3,
  and `Haul` derives `Debug` + `PartialEq` and is asserted on in
  `src/domain/control/tests.rs:1707-1781`, where deterministic `Debug` output is
  worth more than the nanoseconds. Recorded here so the next reader does not
  "fix" it into a pessimisation.
- Nothing anywhere iterates a `HashMap` to produce ordered output. The ordered
  outputs that exist (`retired_keys` at `src/config.rs:227-234`, `removed` at
  `src/config/persist.rs:190-211`, `persisted_sections` at `src/ui/mod.rs:303`,
  `crash_log_paths_from` at `src/crash.rs:75-82`) all use `Vec` with an
  intentional order and `join(", ")`. Correct by construction.

**`coll-set-membership` — every membership test is over a ≤ 8-element collection.**

Exhaustive census of production `.contains(` / `.iter().any(` with the size of the
scanned collection:

- `src/domain/filter.rs:59` `kinds` (≤ 3 real variants), `:66` `names`
  (player-authored, a handful), `:85` `sets` (a handful), `:88-91`
  `required_substats` (a handful), `:127` `item.substats` (≤ 4). All run 6 times
  per snapshot, i.e. a few hundred short compares per shop message at human
  pace. `Vec` also preserves the config file's authored order, which
  `src/config/persist.rs` exists to protect, and the GUI list editor
  (`src/ui/editor/mod.rs:692-720`) renders and mutates them by index — a set
  would break both.
- `src/domain/control/mod.rs:213` `known.contains(&name.as_str())` — `known` is
  always `HAUL_HEADLINERS`, **length 2** (`src/render.rs:26`).
- `src/domain/control/mod.rs:588` `checklist.iter().position(...)` — ≤ 6, and it
  needs the *index* for `swap_remove`, which a set cannot give.
- `src/config.rs:375` `kinds.contains(&ItemKind::Unknown)` — ≤ 3, once at load.
- `src/config/persist.rs:321` `keys.iter().any(...)` (≤ 4) and `:335`
  `headers.contains(...)` (≤ 2), per line of a ~50-line file, once at startup.
- `src/ui/editor/mod.rs:235` `kinds.contains` (≤ 3), `:292` `names.iter().any`
  (2 headliners × a handful of names), `:714` / `:751` on the add-button click
  only.
- `src/ui/editor/mod.rs:146-159` — `hunt_summary` looks each name up in the
  2-entry `HAUL_HEADLINERS` array; a linear scan of a 2-element map, and only
  while the section is folded.
- `src/domain/shop.rs:27` / `src/app/session/mod.rs:378-384` — `slots.iter().find`
  over 6 slots, once per purchase echo.
- `src/crash.rs:86-92` `write_first_writable` over ≤ 2 candidate paths, in
  preference order — a `Vec` because the order *is* the semantics.
- `src/app/session/mod.rs:253,270,662` and every `.iter().any(|line| …contains(…))`
  in `src/app/session/tests.rs` / `src/actuator/mod.rs` tests — assertion helpers
  over ≤ 500 journal lines, once per test.

**`coll-binaryheap` — deliberately and correctly unused.** See *Not applicable*.

## Not applicable

- **`coll-binaryheap`** — no priority queue or repeated max/min-extraction exists,
  and the three sites that superficially resemble one are each better served by
  what they already use:
  - `src/stream.rs:777-789` — `pending.first_key_value()` then `pop_first()` *is*
    repeated min-extraction, but `buffer_future` (`:748-774`) also needs
    `get(&offset)` and `remove(&offset)` for its keep-the-largest dedup at an
    exact offset. `BinaryHeap` supports neither keyed lookup nor arbitrary
    removal (the rule says so itself), so `BTreeMap` strictly dominates it here.
  - `src/stream.rs:641-650` — `evict_stalest` scans a ≤ 64-entry `HashMap` with
    `min_by_key`, only when a *new* flow would exceed `MAX_STREAMS`. It is LRU:
    `last_active` is bumped on every segment (`:570-572`), i.e. a priority
    *update*, which is the one thing `BinaryHeap` cannot do efficiently.
  - `src/domain/control/watchdog.rs:81-125` — the recovery watchdog holds exactly
    one `Option<Expectation>` (`src/domain/control/mod.rs:275`), and deadlines are
    checked from the session loop's 1 s `tokio::time::interval`
    (`src/app/session/mod.rs:50`). There is no "next-due item" collection to
    scan, so nothing for a heap to hold. Same for
    `src/watch.rs:34-38`, which picks the lowest pending halt cause out of a
    2-bit mask via a 2-element array — a bitmask, already better than any heap.
- **`indexmap` / insertion-order maps** — no site needs one: every
  order-sensitive structure is already a `Vec` or a fixed array, and the crate has
  no `indexmap` dependency to weigh (`Cargo.toml:29-68`).
