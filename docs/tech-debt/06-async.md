# 06 — Async/Await (`async-`)

**Category priority:** HIGH
**Rules audited:** 18 (+ `anti-lock-across-await`) · **Files read:** 12 in full, 8 targeted · **Findings:** 6 (P0 0 / P1 1 / P2 4 / P3 1)

## Verdict

The two rules that actually cause bugs are **clean**, and provably so: there is not
one `std::sync::Mutex` guard alive across an `.await` anywhere in the crate (clippy's
`await_holding_lock` is warn-by-default and all four CI lanes run `-D warnings`, and I
traced every guard site by hand on top of that), and every one of the six
`tokio::select!` sites uses only cancellation-safe futures — the two that hold state
across iterations (`drain_until`'s deadline, `forward_chunks`' retag) pin it *outside*
the `select!`, which is exactly the rule's prescribed pattern. `WatchGate` and
`PipelineBudget` even carry written rationales for why their `Notify` waits survive a
cancelled branch.

The worst offender is `src/uplink/websocket.rs`. It is the only module with a real
defect: `connect_async` is awaited with **no timeout**, so a TCP/TLS handshake that
opens but never completes wedges the uplink task forever — the relay looks armed and
healthy, the journal says nothing, and no shop can ever arrive. That module already
proves it understands this failure class (`SEND_TIMEOUT` exists precisely for a
zero-window stall, with a paragraph explaining it); the connect path was simply never
given the same guard. **The single highest-value fix is `async-001`** — wrap the
connect in `tokio::time::timeout` and treat elapsed as a connection failure, which
also bounds the one worker that currently has no cooperative shutdown path at all.

## Findings

### async-001 — `connect_async` is awaited with no timeout: a half-open handshake wedges the uplink forever

- **Severity:** P1
- **Rule:** [`async-select-racing`](../../.claude/skills/rust-skills/rules/async-select-racing.md)
- **Site:** `src/uplink/websocket.rs:110` (`match connect(url.clone()).await`), connector defined at `src/uplink/websocket.rs:87`
- **What:** The reconnect loop's only awaited operation with no bound:

  ```rust
  loop {
      match connect(url.clone()).await {
  ```

  `connect_async` performs DNS resolution, TCP connect, the rustls handshake and the
  WebSocket upgrade, and none of those carry a timeout. A TCP connect to a black-holed
  address does eventually fail (Windows SYN retries, ~21 s), but a connection that
  *establishes* and then stalls — a captive portal, a middlebox that accepts the SYN
  and never speaks TLS, a resolver that never answers — has **no** upper bound. The
  future simply stays pending.
- **Why it matters here:** the failure is silent and total. `outage_reported` is only
  set on the `Err` arm (`websocket.rs:130`), so a hung connect emits no
  `UplinkEvent::LinkDown` — the journal, which is the only surface a windowed-build
  player sees, stays empty. Meanwhile `outbound` (256) fills, `forward_chunks`' final
  `raw_tx.send(chunk).await` (`src/app/mod.rs:854`) parks reassembly, `segment_tx`
  (512) fills, and `capture_loop_budgeted` starts counting pressure drops and
  resyncs. And nothing recovers it from the domain side: `Controller::new` sets
  `link_up: true` (`src/domain/control/mod.rs:298`) and the watchdog only arms behind
  an *issued* refresh, so with no shop ever arriving `expectation` stays `None` and
  the recovery ladder never runs. The player presses Start, reads
  `">> watching — open the shop"`, and then gets silence forever with a WARN-only
  trail in a log file they have to be told to find.
- **Fix:** give connect the same treatment `pump` gives send. A named constant next to
  `SEND_TIMEOUT` documents the pair:

  ```rust
  /// A handshake that cannot finish within this window is a half-open link (captive
  /// portal, middlebox, dead resolver): the same class of stall SEND_TIMEOUT covers,
  /// on the way in. Elapsed is reported and retried like any refused connection.
  const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

  let attempt = tokio::time::timeout(CONNECT_TIMEOUT, connect(url.clone())).await;
  match attempt {
      Ok(Ok(stream)) => { /* … as today … */ }
      Ok(Err(err)) => { /* … as today … */ }
      Err(_elapsed) => {
          warn!(url = %url, "server handshake stalled — retrying");
          if !outage_reported {
              outage_reported = true;
              let _ = inbound
                  .send(UplinkEvent::LinkDown("handshake stalled".to_owned()))
                  .await;
          }
      }
  }
  ```

  The existing `run_retries_on_normalized_schedule_without_network` rig covers this
  cheaply: a connector returning `std::future::pending()` must still reach the second
  attempt on the paused clock.
- **Effort:** small

### async-002 — `pump` awaits the send inside the branch body, so the read half stalls with it

- **Severity:** P2
- **Rule:** [`async-join-parallel`](../../.claude/skills/rust-skills/rules/async-join-parallel.md)
- **Site:** `src/uplink/websocket.rs:183-211` (`pump`), send at `:187-195`
- **What:** the `select!` races `outbound.recv()` against `read.next()`, but the send
  is awaited *inside* the winning branch:

  ```rust
  outgoing = outbound.recv() => match outgoing {
      Some(chunk) => {
          let (bytes, lease) = chunk.into_parts();
          let send = write.send(Message::Binary(bytes.into()));
          match tokio::time::timeout(SEND_TIMEOUT, send).await {
  ```

  For as long as that send takes — up to `SEND_TIMEOUT` = 10 s — `read.next()` is not
  polled. The two halves of one connection are independent futures that the rule says
  should progress concurrently; here one blocks the other head-of-line.
- **Why it matters here:** the delay lands exactly on the watchdog's window.
  `EXPECT_SNAPSHOT_MS` and `EXPECT_PURCHASE_MS` are both `10_000`
  (`src/domain/control/watchdog.rs:10-11`) — the same 10 s. A write stall therefore
  delays the arriving `Shop`/`Purchase` proof by up to the full expectation window,
  and the `Event::Tick` that fires at `deadline_ms` can escalate the ladder before
  the buffered proof is ever read: rung 1 is a free confirm re-click, rung 2 is a
  **full refresh re-issue that spends 3 crystals and re-rolls the shop**, potentially
  discarding a match the player was mid-purchase on. The race with the send timeout's
  own `LinkDown` (which suspends the watchdog, `watchdog.rs:82`) is what keeps this a
  P2 rather than a P1 — it is a narrow window, not a certainty.
- **Fix:** let the halves run concurrently. The natural shape is two futures joined in
  the same task, since `write`/`read` are already the split halves and neither needs
  the other:

  ```rust
  let (mut write, mut read) = stream.split();
  let writer = async { /* while let Some(chunk) = outbound.recv().await { … } */ };
  let reader  = async { /* while let Some(msg) = read.next().await { … } */ };
  tokio::pin!(writer, reader);
  tokio::select! {
      outcome = &mut writer => outcome,
      outcome = &mut reader => outcome,
  }
  ```

  Each arm still returns an `Outcome`, and whichever finishes first ends the
  connection exactly as today. `stalled_send_releases_outbound_bytes_after_timeout`
  keeps guarding the lease release; add a `StalledLink` variant whose reads *are*
  ready to prove an inbound message still lands while a send is stalled.
- **Effort:** medium

### async-003 — the uplink is the one worker with no shutdown signal; teardown reaches it only by `abort`

- **Severity:** P2
- **Rule:** [`async-cancellation-token`](../../.claude/skills/rust-skills/rules/async-cancellation-token.md)
- **Site:** `src/app/mod.rs:348-358` (spawn site), `src/uplink/websocket.rs:92-150`
- **What:** every other worker is told to stop. Capture gets `shutdown_rx`
  (`app/mod.rs:339`), `stdin_loop` gets `shutdown_rx` (`app/mod.rs:406`), the actuator
  watches the gate, reassembly exits on capture EOF. `crate::uplink::run` is handed
  `raw_rx`, `message_tx` and two durations — no `watch::Receiver<bool>` — so its only
  stop condition is `outbound` closing. That closure is observed promptly during
  backoff (`drain_until` selects on `outbound.recv()`, `websocket.rs:161`) but **not**
  while awaiting `connect` (`:110`) or the send (`:188`).
- **Why it matters here:** `SessionWorkers::shutdown` gives all tasks one shared
  250 ms deadline and then aborts (`app/mod.rs:526-554`). In practice the uplink is
  therefore the worker that is routinely aborted mid-handshake on every window close
  that happens while the server is unreachable — and `report_join` deliberately
  suppresses the log line for an expected cancel (`app/mod.rs:558-568`), so the one
  worker that never exits cooperatively is also the one that never says so. It is
  bounded and safe today only because `abort` exists; combined with `async-001` an
  un-timed connect is also an unbounded window in which the cooperative teardown
  simply does not apply.
- **Fix:** thread the existing signal through rather than introducing a new mechanism
  — `ShutdownSignal` is already the crate's cancellation token and every other worker
  takes it. Add `shutdown: watch::Receiver<bool>` to `uplink::run`/
  `run_with_connector`, and race it at the top of the loop and around the two
  unbounded awaits:

  ```rust
  tokio::select! {
      biased;
      _ = shutdown.changed() => return,
      attempt = tokio::time::timeout(CONNECT_TIMEOUT, connect(url.clone())) => attempt,
  }
  ```

  Do not reach for `tokio_util::sync::CancellationToken` here (see Clean areas).
- **Effort:** small

### async-004 — the GUI teardown timeout *detaches* the session handle instead of aborting it

- **Severity:** P2
- **Rule:** [`async-cancellation-token`](../../.claude/skills/rust-skills/rules/async-cancellation-token.md)
- **Site:** `src/main.rs:317-324`; same shape at `src/app/mod.rs:626`
- **What:** the rule's Bad example verbatim — "Dropping handle doesn't stop the task".

  ```rust
  let joined =
      runtime.block_on(async { tokio::time::timeout(TEARDOWN_GRACE, session_task).await });
  if joined.is_err() { tracing::warn!(…); }
  ```

  `timeout` takes `session_task` **by value**, so on the timeout arm the `JoinHandle`
  is dropped — which detaches the task, it does not cancel it. `app::supervise` has
  the same shape one level down: `let outcome = tokio::spawn(session).await;`
  (`app/mod.rs:626`) means that if the supervising future is ever cancelled, the inner
  session task keeps running with nobody holding its handle and `gate.set(false)`
  (`:627`) never executes — capture keeps forwarding and the actuator keeps clicking
  under a session that is officially gone.
- **Why it matters here:** the process does survive this today, because
  `runtime.shutdown_background()` on the next line drops the runtime and cancels
  spawned tasks. But `shutdown_background` explicitly **leaks blocking threads**, so
  in the timeout case the sequence is "detach the task, then leak whatever it was
  parked on" — which is precisely the outcome the warning at `:320-323` describes
  ("a capture session may outlive the process"). Aborting first turns an accident into
  the intended path and shrinks the window in which the gate can still be armed.
- **Fix:** borrow the handle so it survives the timeout, then abort and drain:

  ```rust
  let joined = runtime.block_on(async {
      let outcome = tokio::time::timeout(TEARDOWN_GRACE, &mut session_task).await;
      if outcome.is_err() {
          session_task.abort();          // cancel, do not merely detach
          let _ = session_task.await;    // drain the cancellation
      }
      outcome
  });
  ```

  (`JoinHandle` is `Unpin + Future`, so `&mut session_task` is a valid branch.) In
  `supervise`, hold the inner handle in a value whose `Drop` aborts it — a
  single-element `JoinSet` gives that for free (`async-joinset-structured`: "Abort on
  Drop") — so a cancelled supervisor cannot leave a live session behind.
- **Effort:** trivial (`main.rs`) / small (`supervise`)

### async-005 — the runtime takes every default: worker count unpinned, `block_in_place` invariant left implicit

- **Severity:** P2
- **Rule:** [`async-tokio-runtime`](../../.claude/skills/rust-skills/rules/async-tokio-runtime.md)
- **Site:** `src/main.rs:210-216`
- **What:**

  ```rust
  let runtime = match tokio::runtime::Builder::new_multi_thread()
      .enable_all()
      .build()
  ```

  No `worker_threads`, no `max_blocking_threads`, no `thread_name`. The flavour choice
  itself is right and well argued (`#[tokio::main]` is impossible because eframe/winit
  must own the OS main thread, and `actuator::blocking` needs
  `RuntimeFlavor::MultiThread`, `src/actuator/mod.rs:190-195`). The sizing is what is
  unstated.
- **Why it matters here:** the workload is fixed and tiny — five long-lived tasks
  (uplink, reassembly, actuator, stdin, session loop), all IO-bound or explicitly
  offloaded, plus one `spawn_blocking` at teardown. The default gives one worker per
  `available_parallelism()`, so on a player's 16- or 24-thread gaming CPU the relay
  reserves 16-24 worker stacks to run five tasks, *next to the game it is driving*.
  More importantly the design has a written thread-count assumption —
  `app/mod.rs:515-517`: "with a single worker, that is a deadlock" — and nothing in
  the builder records or enforces it; today it holds only because that one site was
  moved to `spawn_blocking`. This crate names every other thread it creates
  (`"capture"`, `"shield"`, `"pcap-<adapter>"`) precisely because `crash.rs` is the
  product's only post-mortem channel; the worker pool is the one set that stays
  anonymous.
- **Fix:** state the shape the code already depends on.

  ```rust
  let runtime = tokio::runtime::Builder::new_multi_thread()
      // Five long-lived tasks, all IO-bound or offloaded through
      // `actuator::blocking`/`spawn_blocking`. Two workers is the floor the
      // teardown join and `block_in_place` assume; four is the ceiling this
      // workload can use, and the rest would be idle stacks beside the game.
      .worker_threads(4)
      // Only the capture teardown join uses the blocking pool.
      .max_blocking_threads(4)
      .thread_name("relay-worker")
      .enable_all()
      .build()
  ```
- **Effort:** trivial

### async-006 — `run_with_connector` uses the pre-1.85 two-generic future bound instead of `AsyncFnMut`

- **Severity:** P3
- **Rule:** [`async-async-fn-bounds`](../../.claude/skills/rust-skills/rules/async-async-fn-bounds.md)
- **Site:** `src/uplink/websocket.rs:92-103`
- **What:** the rule's Bad pattern, exactly:

  ```rust
  ) where
      C: FnMut(String) -> F,
      F: Future<Output = Result<S, WsError>>,
      S: Stream<Item = Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin,
  ```

  Three generic parameters where two suffice, and the `F` witness is only there to
  name the connector's return future.
- **Why it matters here:** minor, and honestly so — this is readability plus one fewer
  turbofish for future callers, not a bug. It is worth doing because the crate is on
  edition 2024 / `rust-version = "1.92"` (`Cargo.toml:4-5`), so `AsyncFn*` (stable
  since 1.85) is unconditionally available, and because the two-generic form is the
  one that cannot accept an `async ||` borrowing from its environment — a constraint a
  future test connector could easily trip over.
- **Fix:**

  ```rust
  ) where
      C: AsyncFnMut(String) -> Result<S, WsError>,
      S: Stream<Item = Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin,
  ```

  Source-compatible with both existing callers: `run`'s `|url| async move { … }` and
  the tests' `move |_url| { …; ready(Err(…)) }` both satisfy `AsyncFnMut` through the
  std blanket impl over `FnMut` returning a `Future`. Drop the now-unused
  `use std::future::Future;` at `:6`.
- **Effort:** trivial

## Clean areas

**`async-no-lock-await` / `anti-lock-across-await` — clean, and structurally guarded.**
- `Arc<Mutex<Controller>>` is a `std::sync::Mutex` shared between the async session
  loop and the eframe main thread, and that is the right choice: every handler that
  touches it (`session/mod.rs` `heartbeat`, `handle_command`, `on_message`,
  `handle_purchase`, `dispatch`, `apply`) is a **synchronous `fn`**, so the guard
  cannot outlive the call, and most of them `drop(ctrl)` explicitly before touching
  the journal. The doc comment at `session/mod.rs:29` states the invariant.
- `ActuatorHandle::timings`/`set_timings` (`actuator/mod.rs:94-107`) copy `Timings` out
  under the lock; `EventLog::push`/`entries` (`journal.rs:68-107`) hold theirs across
  nothing but a `VecDeque` walk; `PipelineBudget`'s `usage` mutex is released with an
  explicit `drop(usage)` before `notify_waiters()` (`stream.rs:208-209`).
- Verified two ways, not one: I traced every `.lock()` site, and I confirmed
  empirically that `clippy::await_holding_lock` is **warn-by-default** on this
  toolchain (1.92 / clippy 1.97 lint index) with a scratch reproduction, which means
  the four `-D warnings` clippy lanes in `justfile`/`ci.yml` already deny it. Adding
  `[lints.clippy] await_holding_lock = "deny"` to `Cargo.toml`, as the rule's
  Detection section suggests, would be redundant here — do not file it as debt.
- No `RwLock` anywhere in the crate, and no type stores a `MutexGuard` in a field
  (only the two `lock()` helper fns in `shield.rs:32` and `ui/mod.rs:40` return one,
  both from sync callers).
- No `watch::Ref` is held across an await either: all seven `*shutdown.borrow()` reads
  sit in `if` conditions or synchronous loops, where the temporary dies with the
  condition.

**`async-cancel-safety` — clean at all six `select!` sites.** Site by site:
- `app/mod.rs:657` (`reassemble_loop_with_pressure`): `sleep_until(deadline)` with an
  **absolute** deadline recomputed from stored state, `raw_tx.closed()` and
  `events.recv()` — all three cancel-safe, and the accumulating `InitialBurst` lives
  in `AnchorState` *outside* the `select!`, which is the rule's Good pattern verbatim.
- `app/mod.rs:838` (`forward_chunks`): the one non-cancel-safe future here
  (`retag_outbound`, which awaits outbound quota) is `tokio::pin!`-ed and polled as
  `&mut retag`, so the losing iteration keeps its progress. Textbook.
- `app/mod.rs:1043` (`input_loop`): `shutdown.changed()` and `lines.next_line()`. I
  checked the vendored tokio 1.52.3 source rather than assuming — `Lines::next_line`
  is documented "**This method is cancellation safe**" because `buf`/`bytes`/`read`
  live in the `Lines` struct, not in the future. `read_line`/`read_exact` would not
  be; `Lines` is the right adapter to have chosen.
- `session/mod.rs:79`: `gate.halt_requested()`, `shutdown.changed()`, three
  `mpsc::recv()`, `Interval::tick()`, and a `ctrl_c` pinned *outside* the loop so it
  is never re-created or polled after completion. All cancel-safe. The `biased;`
  ordering putting the safety halt first is a correct use of
  [`async-select-racing`](../../.claude/skills/rust-skills/rules/async-select-racing.md)'s
  priority mode. No `else` arm is needed: four of the seven branches are unguarded, so
  the "all branches disabled" panic is unreachable.
- `websocket.rs:159` (`drain_until`): deadline pinned outside the loop, `recv()`
  cancel-safe.
- `websocket.rs:183` (`pump`): both branches cancel-safe (`mpsc::recv`, and
  `SplitStream::next`, whose partial-frame state lives in the `WebSocketStream`). The
  defect there is head-of-line coupling (`async-002`), not lost data.
- `WatchGate::halt_requested` (`watch.rs:110-121`) is cancellation-hardened *by
  design*: the cause lives in a durable `AtomicU8` mask, the wait only wakes, and
  acknowledgement is a separate call the session loop makes after dispatch — the
  comment at `:127-128` says why. Do not "simplify" it into a channel.

**`Notify` usage is correct in both places — do not "fix" it.**
`BudgetedChunk::retag_outbound` (`stream.rs:342-356`) calls
`notified.as_mut().enable()` *before* re-testing `try_retag`, which closes the
lost-wakeup window against `release`'s `notify_waiters()`. `WatchGate` pairs
`notify_one` with a durable atomic. Both look like bugs at a glance and are not.

**`async-bounded-channel` / `async-mpsc-queue` — clean.** Not one
`unbounded_channel` in the crate. Every tokio channel is bounded and sized against a
named concern: commands 16, actuator jobs 8, capture events 512, outbound 256, uplink
events 256, fatal 4. Backpressure is deliberate and *layered* — the 512-slot metadata
queue sits behind `PipelineBudget`'s 32 MiB global / per-stage byte accounting
(`stream.rs:33-36`), which is stronger than message-count bounding alone. The
OS-thread producers use the correct non-async APIs (`try_send`, `blocking_send`) on
tokio channels, and `blocking_send` is only ever called from the plain `std::thread`
capture thread where it cannot panic. One deliberate exception, correctly reasoned and
documented at `capture/pcap.rs:486-491`: the `std::sync::mpsc` between the per-adapter
pcap threads and `next_segment` is unbounded on purpose, because bounding it would
park a capture thread outside the driver and overflow the driver's ring — trading a
transient consumer stall for unrecoverable loss. Both ends are OS threads, so no
executor is blocked. (Residual, not filed: bytes queued there are the only pipeline
bytes `PipelineBudget` does not see.)

**`async-spawn-blocking` — clean, and unusually well reasoned.** Three kinds of
blocking work, three different correct answers: the Npcap receive loop gets a
dedicated named `std::thread` (right for an unabortable long-lived blocking loop, not
`spawn_blocking`); the Win32 click/scroll/acquire/release calls go through
`actuator::blocking` → `block_in_place` with a `RuntimeFlavor` probe so the
current-thread test runtime and the no-runtime guard tests still work
(`actuator/mod.rs:172-195`, with a 24-line comment measuring 120-170 ms per click);
and `capture.stop_and_join()` — the one place a `Thread::join` would park a worker —
is moved to `spawn_blocking` with the single-worker deadlock spelled out
(`app/mod.rs:515-524`).

**`async-tokio-fs` — clean, and `tokio::fs` would be wrong here.** Every `std::fs`
call is outside an async context: `main.rs:44-46` and `migrate.rs` run before the
runtime is built, `crash.rs:100-102` runs in a panic hook, `config/persist.rs` is
called only from `main.rs:177` (pre-runtime) and `ui/mod.rs:231` (the eframe main
thread, which is not a runtime thread). Nothing to convert.

**`async-watch-latest` — clean.** `ShutdownSignal` (`app/mod.rs:185-200`) is a
`watch::Sender<bool>` with `send_replace`, which is exactly the rule's "latest value,
multiple observers, slow receiver skips" case. The consumers get the subtle part right
too: `session_loop:72` and `input_loop:1040` re-read `*shutdown.borrow()` at the top of
the loop instead of trusting `changed()` alone, because a signal set before the loop
started would never fire `changed()` — the comment at `session/mod.rs:69-71` says so.
`shutdown_open = changed.is_ok()` disables the branch when the sender is gone rather
than spinning on the error.

**`async-cancellation-token` — the mechanism choice is right; do not add
`tokio-util`.** The crate has no `tokio_util` dependency and does not need one:
`ShutdownSignal` + `WatchGate` cover cooperative stop and safety halt, there is no
hierarchical/child-token requirement, and a new dependency for a `watch<bool>`
equivalent would be a regression for a single-file shipped exe. The gaps worth fixing
are `async-003` (one worker not wired to the signal) and `async-004` (detach instead
of abort), not the primitive.

**`async-joinset-structured` — the hand-rolled `SessionWorkers` is justified.** It
already implements JoinSet's recipe (shared absolute deadline → `abort` every
unfinished handle → `await` every handle including the cancelled ones,
`app/mod.rs:526-554`) and adds what `JoinSet::join_next` cannot give: a
`&'static str` worker name in every join-failure line, and `report_join`'s
distinction between an expected cancel and a real failure. Migrating would trade
diagnostics for brevity. The three-test suite around it
(`worker_shutdown_cooperative_task_exits_during_grace_and_is_joined`,
`..._pending_tasks_share_deadline_and_abort_is_awaited`,
`..._clean_pipeline_closes_in_producer_order`) pins the behaviour.

**`async-clone-before-await` — clean.** `SessionWorkers::spawn` clones the fatal
sender before the `move` (`app/mod.rs:494`); `setup` hands out `handles.clone()` and
`gate.clone()` per consumer; nothing holds a borrow of `Arc` contents across a
suspension. `MessageSurface` even stores the game window as `isize` rather than
`HWND` specifically so the executor's future stays `Send`
(`actuator/win.rs:564-566`).

**Task supervision is otherwise complete.** All five production `tokio::spawn` sites
are accounted for: four go through `SessionWorkers` (handle retained, panic caught
into the fatal channel, joined or aborted-and-drained), and `main.rs:278`'s handle is
kept explicitly "both the join point for the teardown below and the reason a panic in
this wrapper cannot vanish". Every OS thread is joined too — the capture thread in
`CaptureWorker::stop_and_join`, the per-adapter pcap threads in `PcapSource::drop`.
The one genuinely detached thread is the shield's Win32 message pump
(`actuator/shield.rs:167-177`): unjoined by design, since a window is thread-affine
and is cached in a static for the process lifetime; teardown lowers it via
`shield::hide()` from `MessageSurface::drop`. Not debt.

## Not applicable

- [`async-fn-in-trait`](../../.claude/skills/rust-skills/rules/async-fn-in-trait.md) —
  no async traits and no `#[async_trait]` anywhere. The three `dyn`-used traits
  (`Surface`, `PacketSource`, `CaptureStop`) are deliberately synchronous: their
  methods block, and the executor calls them through `actuator::blocking`. Making them
  `async fn` would break `Box<dyn …>` and lose nothing.
- [`async-try-join`](../../.claude/skills/rust-skills/rules/async-try-join.md) — no
  set of concurrent fallible operations with a shared early-return. The only fallible
  await sequences (connect → pump, acquire → steps) are strictly dependent.
- [`async-oneshot-response`](../../.claude/skills/rust-skills/rules/async-oneshot-response.md)
  — no request/response pattern in the crate. Every channel is one-way fire-and-forget;
  the actuator's "reply" is the game's own wire traffic arriving back through the
  uplink, not a `oneshot`.
- [`async-broadcast-pubsub`](../../.claude/skills/rust-skills/rules/async-broadcast-pubsub.md)
  — nothing needs every subscriber to see every message. `UplinkEvent` has exactly one
  consumer (the session loop), so `mpsc` is correct; the two fan-out signals
  (shutdown, halt) are latest-value/level-triggered, which is `watch` and `Notify`
  territory, not `broadcast`. Introducing one would add lag semantics for no gain.
- `src/domain/**` (`control/mod.rs`, `control/watchdog.rs`, `control/dedup.rs`,
  `filter.rs`, `shop.rs`), `src/config*`, `src/render.rs`, `src/migrate.rs`,
  `src/crash.rs`, `src/capture/ip.rs`, `src/uplink/protocol.rs`, `src/error.rs`,
  `build.rs` — zero `async`/`await`/`tokio` constructs (grep-verified across the whole
  tree). The domain is a pure synchronous state machine driven from the async loop
  under a short-lived lock, which is what makes `async-no-lock-await` easy to satisfy
  here in the first place.
