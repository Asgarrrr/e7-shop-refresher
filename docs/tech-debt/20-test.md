# 20 — Testing (`test-`)

**Category priority:** MEDIUM
**Rules audited:** 15 · **Files read:** 38 · **Findings:** 11 (P0 0 / P1 2 / P2 6 / P3 3)

## Verdict

This is the best-tested crate I have audited in this repository, and the honest
headline is that the *idiom* rules are essentially clean: 518 tests, uniformly
descriptive names, `#[cfg(test)] mod tests` + `use super::*` everywhere, four
feature lanes all running tests in CI, hand-rolled trait fakes at every I/O
seam, and `#[tokio::test(start_paused = true)]` used as the default for anything
touching time. My prime suspects going in — `src/stream.rs`, `src/capture/ip.rs`,
`src/config/persist.rs`, `src/actuator/plan.rs`, `src/uplink/websocket.rs` — are
all well covered, several of them exemplarily so.

The one genuinely dangerous hole is on the **inbound wire path**: no test
anywhere deserializes a `{"type":"shop", …}` envelope, and
`uplink::websocket::forward` — the sole call site that turns server bytes into
`UplinkEvent::Message`, dropping anything undecodable at `debug!` level — has
zero tests, because the only fake stream in the file (`StalledLink`) returns
`Poll::Pending` from `poll_next` forever. Every one of the ~150 tests that feeds
a `ShopSnapshot` into the controller builds it in Rust. So the contract "the
server sends a shop and we decode it" is asserted nowhere, and its failure mode
is the app going permanently, silently blind with a green CI. That is the single
highest-value fix, and it is two small tests. Worst offender file:
`src/uplink/websocket.rs` (8 tests, all on the outbound/backoff half).

Secondary, cheap wins: one test in `src/app/mod.rs` cannot fail (it asserts a
behaviour the paused clock produces either way), `src/crash.rs` still uses
after-the-fact temp-file cleanup that leaks on any assertion failure, and
`egui_kittest`'s `snapshot` feature is enabled in `Cargo.toml` while no snapshot
test exists. Note also that no lane measures coverage — `cargo test` is the only
signal, which is exactly why the `ServerMessage::Shop` gap survived.

## Coverage map

Per-module, "is the risky part tested?" — the headline the rest of this report
hangs off. Counts are `#[test]` + `#[tokio::test]` attributes in the module.

| Module | Tests | Risky part | Covered? |
|---|---:|---|---|
| `src/stream.rs` | 33 | wrap arithmetic, overlap, retransmit, gap fill, >2 GiB offsets, 64-stream eviction, SYN incarnations, budget under/overflow | **Yes** — including a debug/release-split pair on the `release` saturation path and two `#[should_panic]` invariant tests |
| `src/capture/ip.rs` | 8 | malformed/truncated/garbage bytes, IPv6, pure ACK, wrong port | **Yes** — `truncated_bytes_are_rejected` covers half-a-packet *and* arbitrary garbage |
| `src/capture/pcap.rs` | 13 | link-type strip lengths, VLAN/QinQ offsets, short frames, `pcap_pkthdr` size, `caplen` canary | **Yes** — `size_of::<PcapPktHdr>() == 16` is asserted, which is the one FFI mistake that would not crash. FFI itself untestable without Npcap (one `#[ignore]`d live smoke check) |
| `src/config/persist.rs` | 20 | comment preservation, header removal, inline/dotted spellings, retired-key migration | **Yes**, and unusually well — `a_comment_above_a_removed_header_belongs_to_the_line_before_it` asserts *exact text* |
| `src/config.rs` | 45 | atomic-save failure leaves the original intact, `ws://` userinfo bypass, timing-range validation, the bundled example on disk | **Yes** — the filesystem wrappers `save`/`strip_retired_keys` are tested end-to-end here (not only the pure cores), through an RAII `TempDir` |
| `src/actuator/plan.rs` | 31 | aspect/pillarbox transform, jitter bounds, `u64::MAX` range without `% 0`, reversed range | **Yes** |
| `src/actuator/mod.rs` | 23 | epoch/gate abort mid-job, fatal vs recoverable, RAII release during unwind, re-arm after fatal | **Yes** — via a stateful `FakeSurface` |
| `src/actuator/win.rs` | 34 | call *ordering* around UIPI preflight, focus loss between down and up, guaranteed `LeftUp`, LPARAM packing | **Yes** — via a `FakeInputDriver` asserting exact call sequences |
| `src/domain/control/` | 94 | limit priority, dedup fingerprints, gold debiting, watchdog ladder, re-buy guards | **Yes** — the strongest suite in the crate |
| `src/domain/filter.rs` | 29 | fail-closed grade, duplicate substats, `is_unrestricted` arming invariant | **Yes** |
| `src/domain/shop.rs` | 8 | tolerant deserialization (partial/`null`/mistyped/bad array element) | **Yes** for the payload — **but never through the `ServerMessage` envelope** (test-001) |
| `src/app/mod.rs` | 38 | anchor window, burst byte/segment caps, pressure resync protocol, worker teardown ordering | **Yes**, except one vacuous test (test-003) |
| `src/app/session/` | 40 | halt latch vs saturated command queue, watchdog wiring, drop reporting | **Yes** |
| `src/uplink/websocket.rs` | 8 | backoff normalization/overflow, drain during backoff, stalled-send timeout | outbound **yes**; **inbound `pump` arm and `forward()` — no** (test-001, test-002) |
| `src/uplink/protocol.rs` | 4 | `purchase` + unknown-type fallback | `Shop` and `Ack` variants **not decoded anywhere** (test-001) |
| `src/watch.rs` | 6 | halt-cause latch, mask accumulation, stale acknowledgement | latch semantics **yes**; the documented `set(true)`/`request_halt` *interleaving* **no** (test-008) |
| `src/migrate.rs` | 3 | DACL read/reset | **No, and correctly so** — needs an elevated process and a machine with the WinDivert footprint. The three tests pin what *is* testable (target path, constant list, empty report). See "Not applicable" |
| `src/main.rs` | 0 | `seed_config_if_missing` writes the bundled example to `%APPDATA%` on first run | **No** (test-006) |
| `src/ui/**` | 104 | widget wiring, dirty/applied twins, label presence, section tiling | **Yes** — `egui_kittest` harness tests behind `gui`; layout asserted numerically (`hunt.max.y == stop.min.y`) |
| `src/error.rs`, `src/capture/mod.rs`, `src/uplink/mod.rs`, `src/domain/mod.rs` | 0 | type/trait declarations only; `Display` strings asserted indirectly from `config.rs` | acceptable |
| `build.rs` | 0 | the `requireAdministrator` manifest | **Yes, in CI** — `.github/workflows/ci.yml:48` greps the release exe for `requireAdministrator`. A compile-time-invisible product requirement with a real check on the shipped artifact |

**CI lanes** (`.github/workflows/ci.yml`, mirrored by `justfile`): four `cargo test`
lanes — `--no-default-features` (401 tests), `--no-default-features --features
gui,actuator` (505), `--no-default-features --features pcap-backend`, and
`--locked` default (518) — on toolchains `1.92.0` and `stable`, plus four
`clippy --all-targets -D warnings` lanes over the same combinations. Doctests
run (as part of plain `cargo test`) and there are **0** of them. No lane runs
`--ignored`, so the two ignored tests never execute anywhere automatically. No
lane measures coverage.

## Findings

### test-001 — the `ServerMessage::Shop` envelope is never deserialized in any test

- **Severity:** P1
- **Rule:** [`test-descriptive-names`](../../.claude/skills/rust-skills/rules/test-descriptive-names.md) (coverage of the named behaviour) — primary gap, no single rule owns it; nearest is [`test-arrange-act-assert`](../../.claude/skills/rust-skills/rules/test-arrange-act-assert.md)
- **Site:** `src/uplink/protocol.rs:38-84` (test module), `src/uplink/protocol.rs:12-23` (the untested enum)
- **What:** `ServerMessage` is `#[serde(tag = "type", rename_all = "snake_case")]`
  with a newtype variant `Shop(ShopSnapshot)`. The four tests in the module cover
  `{"type":"purchase",…}` (three shapes) and `{"type":"telemetry",…}` → `Unknown`.
  Nothing decodes `{"type":"shop",…}` or `{"type":"ack"}`. Verified:
  `grep -rn 'type":"shop\|type":"ack' src/` returns nothing. Every test that
  exercises a `ShopSnapshot` — in `domain/control/tests.rs`, `app/session/tests.rs`,
  `ui/view.rs`, `ui/shop.rs` — constructs it as a Rust literal, so the whole
  serde path from wire bytes to `Event::Snapshot` is unasserted.
- **Why it matters here:** an internally-tagged newtype variant wrapping a struct
  flattens the struct's fields alongside `"type"`. If the server nests the
  payload instead (`{"type":"shop","shop":{…}}`), or a field name drifts, or the
  tag key changes, `serde_json::from_slice` returns `Err` — and the only call
  site (`websocket::forward`, `src/uplink/websocket.rs:215-222`) answers that with
  `debug!(…, "unrecognized server message, ignored")` and drops it. `debug` is
  below the default `warn` for non-crate targets and the windowed build has no
  console, so the observable symptom is: link established, bytes forwarded,
  heartbeat healthy, `since_last_shop_s` climbing forever, and no error anywhere
  the player can see. `src/config.rs:957` already applies exactly this reasoning
  to `config.example.toml` ("as a bare `&'static str` it can rot … while CI stays
  green"); the shop envelope is the same class of untested contract, with a worse
  failure mode.
- **Fix:** two tests in the existing `protocol::tests` module, reusing its `parse`
  helper:

  ```rust
  #[test]
  fn shop_message_parses_into_a_snapshot() {
      let message = parse(
          r#"{"type":"shop","merchant":"Secret Shop","slots":[
               {"id":102,"slot":3,"kind":"equipment","price":184000,
                "substats":[{"name":"speed","value":15.0}],
                "limit":{"remaining":1,"total":1}}],
             "refresh":{"crystal_balance":95,"cost":3}}"#,
      );
      let ServerMessage::Shop(snapshot) = message else {
          panic!("expected Shop, got {message:?}");
      };
      assert_eq!(snapshot.merchant.as_deref(), Some("Secret Shop"));
      assert_eq!(snapshot.slots.len(), 1);
      assert_eq!(snapshot.slots[0].catalog_id(), Some(102));
      assert_eq!(snapshot.refresh.map(|m| m.cost), Some(3));
  }

  #[test]
  fn ack_message_parses_as_ack() {
      assert!(matches!(parse(r#"{"type":"ack"}"#), ServerMessage::Ack));
  }
  ```

  Consider also asserting `slot_by_id(102)` here, since
  `ShopSnapshot::slot_by_id` (`src/domain/shop.rs:26`) — the haul-recording
  lookup — has no direct test either.
- **Effort:** trivial

### test-002 — `pump`'s inbound arm and `forward()` are unreachable from every test

- **Severity:** P1
- **Rule:** [`test-mock-traits`](../../.claude/skills/rust-skills/rules/test-mock-traits.md)
- **Site:** `src/uplink/websocket.rs:274-279` (`StalledLink::poll_next`), covering `src/uplink/websocket.rs:200-209` and `:215-222`
- **What:** the file has exactly one fake link, and its `Stream` impl is
  `fn poll_next(…) -> Poll<Option<…>> { Poll::Pending }`. Every test that reaches
  `pump` uses it, so the `incoming = read.next()` branch never yields. The
  `Message::Text`, `Message::Binary`, `Message::Close`, `Some(Err(_))` and
  `None` arms — and therefore `forward()` in its entirety, including the
  "undecodable message is silently dropped" policy — are never executed. The
  `run_with_connector` seam (`src/uplink/websocket.rs:92`) already exists and is
  well used for the connect/backoff half; only the stream side is missing a
  scripted double.
- **Why it matters here:** this is the half of the uplink that produces every
  `UplinkEvent::Message` the session loop acts on. Combined with test-001, the
  entire server→client direction is unasserted. `pump` returning
  `Outcome::Disconnected` on `Message::Close` is also what drives the reconnect
  cycle and the `LinkDown` journal line the player reads during an outage — and
  `run_retries_on_normalized_schedule_without_network` only ever exercises
  *connect* failures, not a link that connects then closes.
- **Fix:** add a scripted stream beside `StalledLink`, then three tests. The
  `Sink` half can delegate to a trivially-ready implementation:

  ```rust
  /// Yields a scripted sequence of inbound frames, then pends forever.
  struct ScriptedLink(std::collections::VecDeque<Result<Message, WsError>>);

  impl Stream for ScriptedLink {
      type Item = Result<Message, WsError>;
      fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
          match self.0.pop_front() {
              Some(frame) => Poll::Ready(Some(frame)),
              None => Poll::Pending,
          }
      }
  }
  // Sink<Message>: poll_ready/poll_flush/poll_close => Poll::Ready(Ok(())),
  // start_send => Ok(()).
  ```

  1. a `Message::Text` carrying `{"type":"shop",…}` reaches `inbound` as
     `UplinkEvent::Message(ServerMessage::Shop(_))`;
  2. `Message::Text("not json")` sends **nothing** on `inbound` and does not end
     `pump` (the drop-and-continue policy, currently only a comment);
  3. `Message::Close(None)` returns `Outcome::Disconnected`.
- **Effort:** small

### test-003 — `initial_anchor_downstream_close_does_not_wait_for_deadline` passes either way

- **Severity:** P2
- **Rule:** [`test-tokio-async`](../../.claude/skills/rust-skills/rules/test-tokio-async.md)
- **Site:** `src/app/mod.rs:1712-1727`
- **What:** the test drops `raw_rx`, then asserts only `task.await.unwrap()`. Its
  name claims the `raw_tx.closed()` branch of the `select!` in
  `reassemble_loop_with_pressure` (`src/app/mod.rs:665-668`) short-circuits the
  10 ms anchor deadline. It cannot show that. Under
  `#[tokio::test(start_paused = true)]` the runtime auto-advances the clock
  whenever it has nothing runnable, so with that branch deleted the
  `sleep_until(deadline)` arm would fire instead, `flush_anchor` →
  `forward_chunks` would observe the dropped receiver, return
  `ForwardStatus::Closed`, and the loop would `break` anyway — `task.await`
  still resolves, still `Ok`. The test has no assertion that distinguishes the
  two behaviours.
- **Why it matters here:** the branch exists specifically so teardown does not
  stall a session for the anchor window on every shutdown, and it is the only
  thing standing between "the downstream closed" and "wait 10 ms first". A test
  that cannot fail is worse than none: the next person to simplify that
  `select!` will delete the branch, see green, and ship it.
- **Fix:** assert the elapsed paused-clock time is zero — the idiom is already
  used twice in the same test module
  (`worker_shutdown_cooperative_task_exits_during_grace_and_is_joined`,
  `src/app/mod.rs:2044-2048`, and
  `worker_shutdown_pending_tasks_share_deadline_and_abort_is_awaited`,
  `:2066-2074`):

  ```rust
  tokio::task::yield_now().await;
  let before = Instant::now();
  drop(raw_rx);
  task.await.unwrap();
  assert_eq!(
      Instant::now().duration_since(before),
      Duration::ZERO,
      "a closed downstream must not wait out the anchor window"
  );
  ```
- **Effort:** trivial

### test-004 — `crash.rs`'s temp-file test cleans up after the fact and leaks on failure

- **Severity:** P2
- **Rule:** [`test-fixture-raii`](../../.claude/skills/rust-skills/rules/test-fixture-raii.md)
- **Site:** `src/crash.rs:148-169`
- **What:** `write_first_writable_falls_back_past_an_unwritable_path` creates two
  files in `std::env::temp_dir()` and removes them with two trailing
  `let _ = std::fs::remove_file(…)` calls *after* three `assert!`s. This is
  verbatim the rule's "Bad" example: any failing assertion unwinds past the
  cleanup and leaves `arkyve_nope_<pid>.log` and
  `arkyve_crash_test_<pid>.log` behind. Worse, the blocker file is the premise
  of the test (`append`'s `create_dir_all` must fail on it), so a leaked blocker
  from a previous failing run is fine but a leaked *good* file is not: the test
  asserts `body.contains("entry-one")` on an appending open, so a stale file
  from an earlier run makes a subsequent broken run pass.
- **Why it matters here:** `src/config.rs:1005-1039` already solves this with a
  `TempDir` RAII guard whose doc comment explicitly names this test —
  "*including* when an assertion panics, unlike the hand-rolled after-the-fact
  cleanup in `crash.rs`, which leaks files on every failure". The debt is
  acknowledged in-tree and simply not paid. Both the pid-keyed naming and the
  parallel-collision hazard are already reasoned about there.
- **Fix:** either (a) lift `config.rs`'s `TempDir` into a shared
  `#[cfg(test)]` helper and use it from both sites, or (b) add
  `tempfile = "3"` as a dev-dependency and use `TempDir`/`NamedTempFile`.
  **(b) costs zero build time**: `tempfile` 3.27 is *already* compiled into
  every test build, pulled in by `egui_kittest`'s `snapshot` feature
  (`Cargo.lock:732`) — and if test-005 is resolved by dropping that feature, a
  direct dev-dep is what keeps it. Prefer (b), and then delete
  `config.rs`'s hand-rolled guard too.
- **Effort:** trivial

### test-005 — `egui_kittest`'s `snapshot` feature is enabled and nothing uses it

- **Severity:** P2
- **Rule:** [`test-snapshot-testing`](../../.claude/skills/rust-skills/rules/test-snapshot-testing.md)
- **Site:** `Cargo.toml:95` — `egui_kittest = { …, features = ["wgpu", "snapshot"] }`
- **What:** no snapshot API is called anywhere:
  `grep -rni 'assert_snapshot\|SnapshotResults\|SnapshotOptions\|try_snapshot' src/ examples/`
  returns nothing, and there is no `snapshots/` directory and no `.snap` file in
  the repo. The one consumer of rendering, `render_stop_section_png`
  (`src/ui/editor/mod.rs:1093-1125`), uses `.wgpu()` + `harness.render()` +
  `image.save()`; `Harness::render` is gated
  `#[cfg(any(feature = "wgpu", feature = "snapshot"))]` in egui_kittest 0.35, so
  the `wgpu` feature alone already provides it. The `snapshot` feature therefore
  contributes only `dify`, `open` and `tempfile` — verified as reachable from
  nothing else in the lock (`Cargo.lock:720-733`) — to four `cargo test` lanes
  and four `clippy --all-targets` lanes.
- **Why it matters here:** two things, and the second is the interesting one.
  First, `Cargo.toml:58-63` justifies restricting `eframe` to `glow` because
  "the default wgpu/x11/wayland stacks would only slow the build down", so
  carrying an unused heavy dev-dep feature contradicts a stated goal of the
  manifest. Second, the feature name advertises a capability the crate does not
  have while the test suite is *manually* doing what snapshots are for:
  `collapsed_sections_tile_with_no_hover_gap` (`src/ui/editor/mod.rs:1021-1041`)
  asserts `hunt.max.y == stop.min.y` and `stop.max.y == click.min.y` by hand
  because a layout seam is otherwise invisible. That is a snapshot test written
  as three coordinate comparisons.
- **Fix:** decide, and make the manifest say what is true.
  - **Drop it** (recommended if nobody will maintain golden images): change to
    `features = ["wgpu"]`, add `tempfile` as a direct dev-dep per test-004, and
    keep `render_stop_section_png` as the deliberate developer tool it is.
  - **Use it**: convert `render_stop_section_png` from an `#[ignore]`d PNG
    dumper into `harness.snapshot("setup_stop_section")` with the `.png` golden
    committed, and add a lane that runs it. Cost to weigh honestly: golden
    images are font- and GPU-renderer-sensitive, `windows-latest` runners change
    GPU stacks between images, and this would be the first artifact in the repo
    that a CI image bump can break for reasons unrelated to the code. Given
    the 104 kittest tests already assert behaviour via the accessibility tree,
    the marginal value is layout-only.
  Either way, note that no lane runs `--ignored`, so `render_stop_section_png`
  currently never executes and can rot silently while being the sole
  justification for `wgpu`.
- **Effort:** trivial (drop) / medium (adopt)

### test-006 — `src/main.rs` has no test module; `seed_config_if_missing` is untested

- **Severity:** P2
- **Rule:** [`test-cfg-test-module`](../../.claude/skills/rust-skills/rules/test-cfg-test-module.md)
- **Site:** `src/main.rs` (340 lines, 0 tests) — specifically `seed_config_if_missing` at `:38-47`, `config_path` at `:22-30`, `log_dir` at `:52-58`
- **What:** the binary crate root has no `#[cfg(test)] mod tests`. There is no
  structural reason for that — `cargo test` builds and runs bin targets, and CI's
  test invocations do not pass `--lib`, so tests there would run in all four
  lanes. `seed_config_if_missing` is the one with product consequence: it writes
  `include_str!("../config.example.toml")` to the resolved config path on first
  run, and it takes a `&Path`, so it is directly testable.
- **Why it matters here:** `config.rs:957`'s
  `bundled_example_config_parses_validates_and_is_restrictive` goes to
  considerable trouble to prove the *content* of the example is sound
  (parses, validates, is restrictive, plants no retired key, survives
  `strip_retired_keys` byte-identically) precisely because "the shipped exe hands
  100% of new players an 'Invalid configuration' window before they see the app".
  The function that actually performs that write — including its
  `if path.exists() { return; }` guard, i.e. *never overwrite a player's file* —
  is not covered by any of it. A regression that made it truncate an existing
  config would destroy the player's settings and no test would notice.
- **Fix:** add a `#[cfg(test)] mod tests` to `src/main.rs` using the shared
  RAII temp-dir helper from test-004:

  ```rust
  #[test]
  fn seeding_creates_the_parent_and_writes_the_bundled_example() { /* absent dir → file == EXAMPLE */ }

  #[test]
  fn seeding_never_overwrites_an_existing_config() { /* pre-write "# mine\n" → unchanged */ }

  #[test]
  fn log_dir_and_config_path_end_in_the_shared_app_dir() { /* both contain APP_DIR */ }
  ```

  `log_dir`/`config_path` read process env, so keep those assertions to
  path-shape only (an `EnvGuard` would need `unsafe` and single-threaded
  execution — not worth it for two `join` calls).
- **Effort:** small

### test-007 — the byte-parsing and coordinate-transform code has no property tests

- **Severity:** P2
- **Rule:** [`test-proptest-properties`](../../.claude/skills/rust-skills/rules/test-proptest-properties.md)
- **Site:** `src/actuator/plan.rs:130-155` (`to_screen`), `src/capture/ip.rs:20` (`parse_segment`), `src/stream.rs:834` + `:556` (`seq_diff`, `Reassembler::push_budgeted`), `src/actuator/win.rs:1484` (`pack_point` roundtrip)
- **What:** four functions are pure, total, and defined by algebraic properties,
  and all four are currently pinned by hand-enumerated examples. Two of them are
  visibly *straining* against that:
  `initial_anchor_all_six_permutations_keep_the_immediate_suffix`
  (`src/stream.rs:1149-1171`) enumerates 3! = 6 arrival orders of three
  hard-coded segments, and `pack_point_round_trips_through_get_x_y_lparam`
  (`src/actuator/win.rs:1484-1497`) is a hand-written roundtrip property over
  five points, complete with the inverse transform spelled out in the test body.
- **Why it matters here:** `to_screen` is the highest-value target. It is the
  only thing between a decoded shop and a synthetic click landing on the right
  pixel, its five tests cover five hand-picked resolutions (1280×720, 1920×1080,
  1440×720, 3440×1440, and two rejections), and a player's window can be any
  size at all. A wrong coordinate does not fail loudly — it clicks the wrong
  button in a shop that spends real currency. `parse_segment` is second: it eats
  attacker-adjacent bytes off the wire (capture is port-wide, so any host sending
  from port 3333 reaches it) and its malformed-input coverage is two cases.
- **Fix:** add `proptest = "1"` as a dev-dependency and three property blocks.
  Sketches:

  ```rust
  // plan.rs — the invariant the executor relies on and no example test states.
  proptest! {
      #[test]
      fn every_design_point_maps_inside_a_valid_client_area(
          left in -4000i32..4000, top in -4000i32..4000,
          height in 200i32..2200, extra in 0i32..3000,
          x in 0.0f32..=1280.0, y in 0.0f32..=720.0,
          anchor in prop::sample::select(vec![Anchor::Left, Anchor::Right, Anchor::Center]),
      ) {
          // At least 16:9 by construction, so to_screen must accept it.
          let width = (height as f32 * DESIGN_W / DESIGN_H).ceil() as i32 + extra;
          let rect = ClientRect { left, top, width, height };
          let (px, py) = to_screen(rect, DesignPoint { x, y, anchor }).unwrap();
          prop_assert!((left..=left + width).contains(&px));
          prop_assert!((top..=top + height).contains(&py));
      }

      #[test]
      fn pillarbox_bars_are_symmetric(height in 200i32..2200, extra in 1i32..3000) { /* left_edge == width - right_edge */ }
  }

  // capture/ip.rs — totality on arbitrary input. Cheap, and the current
  // coverage is "half a valid packet" plus one string literal.
  proptest! {
      #[test]
      fn parse_segment_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..2048), port in any::<u16>()) {
          let _ = parse_segment(&bytes, port);
      }
  }

  // stream.rs — generalizes the six-permutation table to any n and any origin,
  // including across the u32 wrap.
  proptest! {
      #[test]
      fn any_arrival_order_yields_a_suffix_of_the_original_stream(
          origin in any::<u32>(),
          order in prop::sample::subsequence((0..8usize).collect::<Vec<_>>(), 8).prop_shuffle(),
      ) {
          // Build 8 contiguous 2-byte segments from `origin`, push in `order`,
          // concatenate the outputs, assert the result is a suffix of
          // b"ABCDEFGHIJKLMNOP" — the documented rule in
          // docs/initial-stream-anchor.md, currently proven for n=3 only.
      }
  }
  ```

  **Cost:** `proptest` pulls roughly eight test-only crates (`proptest`,
  `bit-set`, `bit-vec`, `rand`/`rand_chacha`/`rand_core`, `rusty-fork`,
  `unarray`, `wait-timeout`, `quick-error`). That is real compile time on four
  lanes for a crate that already has 518 example tests, so it is only worth it if
  scoped: I would add it for `to_screen` and `parse_segment` (where the input
  space is genuinely unbounded and the consequences are wrong clicks and panics
  on hostile bytes) and leave `domain/filter.rs` and `domain/control` alone,
  where the example suites already enumerate the decision table exhaustively and
  a generator would mostly restate the implementation. Note `proptest` regressions
  are written to a `proptest-regressions/` file that must be committed —
  budget for that in `.gitignore` review.
- **Effort:** medium

### test-008 — `WatchGate`'s documented store/latch races are only tested sequentially

- **Severity:** P2
- **Rule:** [`test-loom-concurrency`](../../.claude/skills/rust-skills/rules/test-loom-concurrency.md)
- **Site:** `src/watch.rs:72-107` (the double-store dance), tested by `src/watch.rs:140-217` (6 sequential tests)
- **What:** `WatchGate::set` stores `enabled = true`, re-reads `pending_halt`,
  and stores `false` again "to close the race with a request that starts between
  the first check and the enabled store" (`:69-71`); `request_halt` mirrors it,
  storing `enabled = false` *twice* around its `fetch_or`, "to close the race
  with `set(true)` if it observed an empty mask before this cause was published"
  (`:103-105`). Both comments describe a two-thread interleaving. All six tests
  call the methods in a fixed order on one thread, so they verify the *latch
  semantics* (mask accumulation, lowest-bit priority, stale-ack immunity) and
  never an interleaving. The same is true of `app::PressureResync`
  (`src/app/mod.rs:72-131`), a three-state `compare_exchange` protocol whose
  test (`capture_pressure_counts_bytes_and_queues_one_resync`,
  `src/app/mod.rs:1327`) drives it strictly sequentially.
- **Why it matters here:** this is the *safety* gate. It is what a fatal actuator
  fault uses to stop clicking synchronously without going through the bounded
  command queue, and `saturated_command_queue_cannot_drop_an_actuator_halt`
  (`src/app/session/tests.rs:188`) exists because that path must not be lossy.
  The redundant stores are a hand-proof; nothing checks the proof.
- **Fix:** scope loom to `WatchGate` only. Its state is two atomics
  (`AtomicBool`, `AtomicU8`) plus a `Notify`, and the interesting model is two
  threads — `set(true)` against `request_halt(ActuatorFailed)` — asserting the
  post-join invariant "if a cause is latched then `enabled == false`". That state
  space is small enough for loom to explore quickly.

  ```rust
  // src/watch.rs
  #[cfg(loom)]
  use loom::sync::atomic::{AtomicBool, AtomicU8, Ordering};
  #[cfg(not(loom))]
  use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
  ```

  ```rust
  #[cfg(loom)]
  #[test]
  fn a_halt_racing_a_rearm_always_wins() {
      loom::model(|| {
          let gate = WatchGate::new(false);
          let rearm = { let g = gate.clone(); loom::thread::spawn(move || g.set(true)) };
          gate.request_halt(HaltSource::ActuatorFailed);
          rearm.join().unwrap();
          assert!(!gate.is_enabled(), "a latched cause must never leave the gate armed");
      });
  }
  ```

  **Cost, honestly:** loom needs (a) the `#[cfg(loom)]` import swap above, which
  touches production code in a file whose atomics are load-bearing; (b) `Notify`
  has no loom equivalent, so `halt_requested` must stay out of the model — only
  the atomic pair can be checked; (c) a fifth CI lane
  (`RUSTFLAGS="--cfg loom" cargo test --test loom_gate`) that no existing lane
  covers. Against that: both racing paths already store `false`, so the gate is
  fail-safe by construction and a missed interleaving costs an unnecessary halt,
  not a click. I would do this when next touching `watch.rs`, not before. Decline
  loom for `PipelineBudget` (a `Mutex` guards all of it — nothing lock-free to
  model) and for `SnapshotEpoch` (a single `fetch_add`).
- **Effort:** medium

### test-009 — three tests wait on the real clock where a paused clock would be deterministic

- **Severity:** P3
- **Rule:** [`test-tokio-async`](../../.claude/skills/rust-skills/rules/test-tokio-async.md)
- **Site:** `src/app/session/tests.rs:109`, `src/app/session/tests.rs:393` (both `#[tokio::test]`, no `start_paused`), `src/journal.rs:141` (`std::thread::sleep(30ms)` in a sync test)
- **What:** `a_worker_panic_is_reported_when_the_uplink_channel_closes` spawns a
  task that `tokio::time::sleep(10ms)`s before sending the fatal, so the report
  lands inside the loop's 150 ms post-loop grace window
  (`src/app/session/mod.rs:165-173`). `shutdown_signal_ends_the_loop_and_stops_the_watch`
  does the same to delay the shutdown signal. `timestamps_track_elapsed_time`
  sleeps 30 ms of wall clock so two journal stamps differ. The rest of the crate
  is disciplined about this — 30 of 54 async tests use
  `#[tokio::test(start_paused = true)]`.
- **Why it matters here:** two of the three are ordering races expressed as
  durations. `10ms` vs `150ms` is a wide margin, but it is a margin, and these
  run on shared CI runners across eight lane×toolchain combinations; a stalled
  runner turns a deliberate race into a flake in the one test whose job is to
  prove a crash is not misreported as a clean exit. Adding
  `start_paused = true` makes both deterministic rather than merely probable:
  the paused runtime advances to the 10 ms deadline as soon as everything is
  idle, so the send provably happens before the 150 ms timeout expires.
- **Fix:** add `(start_paused = true)` to the two `#[tokio::test]` attributes at
  `src/app/session/tests.rs:91` and `:376`; no body change needed. For
  `journal.rs:141`, `EventLog` uses `std::time::Instant` (not
  `tokio::time::Instant`), so a paused clock is unavailable — leave it, or drop
  the sleep and weaken the assertion to `entries[1].at_ms >= entries[0].at_ms`,
  which is what the test's own comment ("a later push carries a later stamp")
  actually needs.
- **Effort:** trivial

### test-010 — a handful of tests bundle several independent scenarios

- **Severity:** P3
- **Rule:** [`test-arrange-act-assert`](../../.claude/skills/rust-skills/rules/test-arrange-act-assert.md)
- **Site:** `src/domain/control/tests.rs:1018-1053` (`stop_reason_priority_order`, four scenarios with four separate arranges), `src/domain/control/tests.rs:1004-1015` (`out_of_funds_when_balance_below_cost`, two), `src/config.rs:1258-1273` (`scheme_match_is_case_insensitive`, three), `src/config.rs:942-954` (`ws_userinfo_loopback_is_rejected`, two), `src/uplink/websocket.rs:239-255` (`backoff_normalizes_floor_and_cap`, two independent `Backoff`s)
- **What:** each of these constructs a fresh subject two-to-four times and runs a
  full arrange/act/assert cycle per block. They are readable — every block has an
  explanatory comment and the sub-cases are genuinely related — but a failure
  reports one name for four behaviours, and the first failing block hides the
  rest.
- **Why it matters here:** `stop_reason_priority_order` is the sharpest case: it
  pins the *ordering* of five halt conditions, which is exactly the kind of thing
  that gets reordered by accident in `stop_reason` (`src/domain/control/mod.rs:677`),
  and a failure says only "priority order broke" without saying which pair. The
  crate's own convention elsewhere is one behaviour per name
  (`max_matches_boundary_uses_ge`, `max_spend_is_hard_ceiling_no_overshoot`, …),
  so these are the outliers, not the norm.
- **Fix:** for `stop_reason_priority_order`, keep one test but make it
  table-driven so the failure names the pair — e.g. iterate
  `[(limits, expected_reason, "label")]` and pass the label into the
  `assert_eq!` message. For the others, split along the comment boundaries that
  already exist (`out_of_funds_when_balance_below_cost` +
  `balance_equal_to_cost_still_affords_one_refresh`). Batchable; do it when next
  editing these files.
- **Effort:** small

### test-011 — no `tests/` directory: correct overall, with one candidate worth extracting

- **Severity:** P3
- **Rule:** [`test-integration-dir`](../../.claude/skills/rust-skills/rules/test-integration-dir.md)
- **Site:** repository root (no `tests/`); candidate content at `src/config.rs:1041-1196`
- **What:** there is no `tests/` directory and no integration-test target, so
  `cargo test` compiles only lib, bin and example targets. Taking the rule at
  face value this is a gap; taking the crate seriously, it is mostly the right
  call and should be recorded as such so nobody "fixes" it.

  **Why it is mostly right here.** The crate is `publish = false` and ships as a
  binary; `src/lib.rs` exists so `main.rs` and the tests can share code, not to
  offer an API. The rule's stated benefit — "testing your library's public
  interface as external users would" — has no external users to model. And the
  cost is concrete: an integration test can only reach `pub` items, while nearly
  every interesting seam in this crate is `pub(crate)` or private on purpose
  (`PipelineBudget`, `BudgetedSegment`, `InitialBurst`, `CaptureSource`,
  `reassemble_loop_with_pressure`, `session_loop`, `handle_command`,
  `write_sections`, `strip_sections`, `Backoff`, `pump`). Moving any of those
  tests to `tests/` would mean widening visibility for the tests' benefit, which
  is worse debt than the missing directory. The two large `tests.rs` submodules
  (`src/domain/control/tests.rs`, `src/app/session/tests.rs`) are the right
  answer to "the test module got big": `#[cfg(test)] mod tests;` in the parent
  plus `use super::*;` at the top of the file, keeping private access.

  **The one candidate.** `src/config.rs`'s test module contains five tests that
  are not unit tests of `Config` at all — they are a filesystem lifecycle test of
  a *different* module: `save_then_load_round_trips_the_edited_sections_through_disk`,
  `stripping_a_players_config_clears_the_retired_warning_for_good`,
  `a_failed_strip_leaves_the_retired_keys_in_place_to_warn_about`,
  `a_failed_save_leaves_the_original_config_intact`,
  `stripping_a_missing_config_is_not_an_error`. They reach across into
  `crate::config::persist` by fully-qualified path, carry their own `TempDir`
  fixture, and exercise seed → load → strip → save → reload end to end. Every
  API they touch is already `pub` (`Config::load`, `persist::save`,
  `persist::strip_retired_keys`, `persist::Section`), so `tests/config_lifecycle.rs`
  would compile unchanged.
- **Why it matters here:** low. The tests work where they are. The argument for
  moving them is that they would then also prove the surface `main.rs` actually
  uses is reachable from outside the crate, and they would stop inflating
  `config.rs`'s test module (45 tests) with concerns that are not about parsing
  `Config`. The argument against is that a new target adds a link step to four
  lanes for five tests. My call: worth doing if and only if test-006 lands, since
  a `tests/common/mod.rs` would then be the natural home for the shared
  `TempDir`/`tempfile` fixture that `config.rs`, `crash.rs` and `main.rs` all
  need.
- **Fix:** optional. Move the five named tests to `tests/config_lifecycle.rs`,
  with the temp-dir fixture in `tests/common/mod.rs`. Leave everything else in
  `src/`.
- **Effort:** small

## Clean areas

- [`test-cfg-test-module`](../../.claude/skills/rust-skills/rules/test-cfg-test-module.md) — every one of the 29 test-bearing files uses `#[cfg(test)] mod tests`, and no test code is reachable in a release build. The two oversized suites use the file-submodule form (`#[cfg(test)] mod tests;` at `src/domain/control/mod.rs:10` and `src/app/session/mod.rs:672`) rather than inflating the parent — the pattern the rule's "Multiple Test Modules" section describes. Test-only helpers are correctly gated too: `Filter::matching_default_items` (`src/domain/filter.rs:110`), `PipelineBudget::with_test_limits` (`src/stream.rs:111`), `admit_outbound_for_test` (`:261`), `Reassembler::push` (`:605`), `flatten_chunks` (`:815`), and the `Debug`/`PartialEq<Vec<u8>>` impls on `BudgetedChunk` (`:364`, `:371`) that exist only so assertions read well.
- [`test-use-super`](../../.claude/skills/rust-skills/rules/test-use-super.md) — `use super::*;` in every test module without exception, including both standalone `tests.rs` files (line 1 of each) and the nested `super::super::view` imports in the UI modules (`src/ui/statusbar.rs:168`, `src/ui/shop.rs:180`). Private-item access is used, not worked around: tests read `r.streams.len()`, `half.baseline`, `half.syn_seq`, `half.next_off`, `surface.target`.
- [`test-descriptive-names`](../../.claude/skills/rust-skills/rules/test-descriptive-names.md) — the strongest area in the audit. Not one `test1`, `it_works`, or bare `test_parse` in 518 tests. Names state the behaviour and often the reason: `a_segment_sent_to_the_game_port_is_not_a_segment_at_all`, `release_underflow_saturates_instead_of_panicking`, `a_comment_above_a_removed_header_belongs_to_the_line_before_it`, `an_unknown_link_type_yields_no_strip_so_the_device_is_skipped_rather_than_guessed_at`, `a_config_written_before_the_capture_keys_were_retired_still_loads`. Several carry doc comments explaining the regression they exist for (`src/config.rs:445`, `src/config/persist.rs:784`, `src/stream.rs:946`), which is better than the rule asks for.
- [`test-arrange-act-assert`](../../.claude/skills/rust-skills/rules/test-arrange-act-assert.md) — structurally honoured throughout via arrange helpers (`started(limits)`, `recovering(limits)`, `rig()`, `fake_surface()`, `controller()`, `equip()`, `hunt_filter()`, `run_setup()`) and assert helpers (`assert_within`, `click_at`, `scroll_notches`, `sent_events`, `validation_calls`, `recv_exact`), which is exactly the rule's "Helper Functions" pattern. Explicit `// Arrange / // Act / // Assert` comments are absent, but the three phases are visually separated by blank lines in the async tests and the helpers make the boundary obvious. Only the outliers in test-010 depart from this.
- [`test-tokio-async`](../../.claude/skills/rust-skills/rules/test-tokio-async.md) — no hand-rolled `Runtime::new().block_on(…)` anywhere; 54 `#[tokio::test]`s, 30 of them `(start_paused = true)`. The flavor argument is used deliberately and the reason is documented: `blocking_offloads_on_the_multi_thread_runtime` is `(flavor = "multi_thread", worker_threads = 2)` because `block_in_place` panics elsewhere, with a matching current-thread test and a no-runtime test beside it (`src/actuator/mod.rs:430-443`, rationale at `src/actuator/mod.rs:186-189`). `tokio::time::advance` drives every anchor-window test, and `tokio::time::timeout` is used as a liveness assertion rather than a sleep (`src/actuator/mod.rs:711`, `:757`, `:810`).
- [`test-should-panic`](../../.claude/skills/rust-skills/rules/test-should-panic.md) — used exactly twice, both correctly and both with `expected = "…"`: `src/stream.rs:968` and `:976`, on `debug_assert!`/`assert!` invariant violations in the pipeline accounting. And the discrimination the rule asks for is present: `release_underflow_saturates_instead_of_panicking` is `#[cfg(not(debug_assertions))]` while `release_underflow_fails_fast_in_debug_builds` is `#[cfg(debug_assertions)]` + `#[should_panic]`, so the *same* code path is asserted to panic in one profile and to saturate in the other — with the reason (a panic in `Drop` during unwind aborts without a crash log) documented at `src/stream.rs:182-191`. Nowhere is `#[should_panic]` misused for a recoverable error; those all return `Result` and are asserted with `matches!`.
- [`test-fixture-raii`](../../.claude/skills/rust-skills/rules/test-fixture-raii.md) — honoured everywhere except `crash.rs` (test-004). `TempDir` with a `Drop` impl at `src/config.rs:1010-1039`; `LiveGuard` (`src/app/mod.rs:1248-1261`) counts live sources/tasks and is asserted to reach zero after teardown; and the pattern is applied to *production* code as well — `SurfaceJobGuard` (`src/actuator/mod.rs:198-238`) with a test proving it releases exactly once during a `catch_unwind` panic (`src/actuator/mod.rs:544-555`).
- [`test-mock-traits`](../../.claude/skills/rust-skills/rules/test-mock-traits.md) — every I/O boundary is a trait with a test double: `PacketSource`/`CaptureStop` (five fakes in `src/app/mod.rs`: `EnableOnFirstSegment`, `LosingSource`, `BlockingSource`, `BlockingStop`, `ImmediateErrorSource`), `Surface` (`FakeSurface` with scripted `deny_after` and an `on_input` callback that mutates the gate mid-job), `InputDriver` (`FakeInputDriver` recording an exact `Vec<DriverCall>`), and the WebSocket connector injected as a closure through `run_with_connector`. The `Surface` trait's doc comment even names the test double as a first-class implementation ("real input on Windows, a recorder in tests", `src/actuator/mod.rs:144`). This is what the rule asks for, done throughout.
- CI feature coverage — all four meaningful feature combinations run tests, not just clippy, on two toolchains, and the `justfile` mirrors them so the same lanes are reachable locally (`just verify` for the portable ones, `just backends` for the Windows ones). The comments in both files explain why there is no `--all-features` lane. Untestable-in-CI behaviour is not faked but asserted where it is observable: the `requireAdministrator` manifest is greped out of the shipped release exe (`.github/workflows/ci.yml:48-58`) because it is invisible to every compile-time check.

## Not applicable

- [`test-mockall-mocking`](../../.claude/skills/rust-skills/rules/test-mockall-mocking.md) — the four dependency traits are already mocked by hand, and the doubles do things `mockall`'s expectation builder expresses poorly: `FakeSurface.on_input` is a `Box<dyn FnMut()>` that flips the shared gate or bumps the epoch *between* recorded inputs; `FakeInputDriver` accumulates a `Vec<DriverCall>` that tests assert against positionally (`&actual[..7]`) to pin call *ordering* around the UIPI preflight; `FakeState` drives five independent `VecDeque` scripts. Adding `mockall` would mean either rewriting working stateful fakes into a less expressive form or carrying a proc-macro dependency used nowhere. Recommend against.
- [`test-criterion-bench`](../../.claude/skills/rust-skills/rules/test-criterion-bench.md) — no `benches/`, and none warranted. The per-packet hot path is `Reassembler::push_budgeted` and `InitialBurst::into_ordered`, but the workload is one game connection at a few KB/s behind a kernel BPF filter that admits a single port's half-stream, with an 8 MiB per-stream pending cap and a 32 MiB global one. Throughput is not a product concern; *memory per packet* is, and that already has a guard better suited to it than a benchmark — the `const _` size canaries at `src/stream.rs:418-424` and `src/capture/mod.rs:77-83` fail the build if `FlowKey`, `Segment`, `BudgetedChunk` or `BudgetedSegment` grow, with comments explaining that a failure means "re-measure deliberately". A criterion suite would add a dev-dependency and a `harness = false` target to protect a property nobody has complained about. Recommend against; revisit only if the capture set ever widens beyond one port.
- [`test-doctest-examples`](../../.claude/skills/rust-skills/rules/test-doctest-examples.md) — `cargo test --doc` reports `0 tests`, and that is defensible. The crate is `publish = false` with no external consumers, so the rule's dual purpose ("demonstrating usage to readers *and* verifying the examples") collapses to the second half, which the 518 unit tests already do better. The two fenced blocks that exist are correctly marked non-executable: `src/lib.rs:9-13` is a ```` ```text ```` pipeline diagram and `src/capture/pcap.rs:1193-1195` is a ```` ```text ```` shell command line for running the ignored live smoke test. Neither is a Rust example pretending to compile — which is the failure mode the rule targets. Two `pub` items (`plan::to_screen`, `render::grouped`) could carry doctests, but they would restate existing unit tests for an audience that does not exist. No finding.
- Elevated / hardware-dependent paths — `src/migrate.rs`'s `dacl_is_protected` and `reset_dacl_to_inherited` genuinely cannot be unit-tested: they need a directory carrying `SE_DACL_PROTECTED` (only a WinDivert-era install produces one) and an administrator token to undo it, and `SetNamedSecurityInfoW` against a fixture directory would be a destructive test of the developer's own `%LOCALAPPDATA%`. The three tests present cover what is testable — that the target is the app-data leaf and not its parent (`the_cleanup_targets_the_app_data_root_and_never_its_parent`, which is the dangerous mistake, since the reset propagates downward), that a default `Leftovers` reports nothing, and that the three filenames are spelled correctly now that the module which wrote them is gone. That is the right scope; say so rather than filing a finding. Likewise `PcapSource::open` needs Npcap and a real adapter, and is covered by an `#[ignore]`d smoke check with the invocation documented (`src/capture/pcap.rs:1189-1203`) — though note nothing runs `--ignored` anywhere, so it only executes when a developer remembers to.
