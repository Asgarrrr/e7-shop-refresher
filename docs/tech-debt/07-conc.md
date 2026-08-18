# 07 — Concurrency (`conc-`)

**Category priority:** HIGH
**Rules audited:** 4 · **Files read:** 17 (+ a full-crate sweep for `Atomic`, `Ordering`, `Mutex`, `RwLock`, `Cell`, `RefCell`, `static mut`, `thread_local!`, `thread::`, `Once`, `Condvar`, `fence`, `unsafe impl Send/Sync`, `rayon`, so every synchronisation site in all 38 `.rs` files is accounted for) · **Findings:** 7 (P0 0 / P1 1 / P2 0 / P3 6)

## Verdict

The concurrency design of this crate is deliberate and, with one exception, correct: there is no `static mut` anywhere, the one process-global is a `Mutex` with a documented poison policy, `Once` guards the DPI call, no `std` guard is ever held across an `.await`, and the `Notify` handshake in `stream.rs` registers before it re-checks (the textbook lost-wakeup fix). The exception is the worst offender and the single highest-value fix: **`src/watch.rs`'s halt/re-arm handshake is a two-variable Dekker protocol in which only one of the two variables is `SeqCst`** — `WatchGate::set(true)` can therefore re-arm the safety gate *after* `request_halt` latched a cause, which is exactly the outcome its double-check was written to prevent (`conc-001`). Everything else in the category is the cheap direction of the same rule: seven atomics carry `AcqRel`/`Release`/`Acquire`/`SeqCst` where they publish no other write and `Relaxed` provably suffices. `conc-thread-local` and `conc-rayon-par-iter` are clean/inapplicable, and a deliberate lock-ordering audit found exactly one nesting in the crate (`controller` → `timings`) taken in one direction only, so there is no deadlock cycle.

## Findings

### conc-001 — `WatchGate`'s halt/re-arm handshake spans two atomics but only one is `SeqCst`, so a safety halt can be silently re-armed

- **Severity:** P1
- **Rule:** [`conc-atomic-ordering`](../../.claude/skills/rust-skills/rules/conc-atomic-ordering.md)
- **Site:** `src/watch.rs:72-87` (`set`), `src/watch.rs:98-107` (`request_halt`); readers at `src/watch.rs:64` (`is_enabled`), callers at `src/ui/mod.rs:253`, `src/actuator/mod.rs:402`, `src/app/session/mod.rs:448`
- **What:** `set(true)` and `request_halt` are each "store my flag, then read the other side's flag" — the classic Dekker/store-load pattern, and the comments say so ("The second check closes the race with a request that starts between the first check and the enabled store", "Close the race with `set(true)` if it observed an empty mask before this cause was published"). But the two variables carry different strengths:

  ```rust
  // set(true), on the session-loop task:
  if self.inner.pending_halt.load(Ordering::SeqCst) != NO_HALT { … }
  self.inner.enabled.store(true, Ordering::Release);          // <- not SeqCst
  if self.inner.pending_halt.load(Ordering::SeqCst) != NO_HALT {
      self.inner.enabled.store(false, Ordering::Release);
  }

  // request_halt, on the egui main thread:
  self.inner.enabled.store(false, Ordering::Release);         // <- not SeqCst
  self.inner.pending_halt.fetch_or(source as u8, Ordering::SeqCst);
  self.inner.enabled.store(false, Ordering::Release);         // <- not SeqCst
  ```

  **The happens-before edge this is supposed to create, and does not.** The invariant wanted is: *once `request_halt` returns, `enabled` is `false` and stays `false` until `acknowledge_halt`.* A `Release` store followed by a `SeqCst` load is not ordered against each other — `Release` only forbids hoisting *earlier* operations past it, so both LLVM and x86-TSO (store buffer) may satisfy the `pending_halt` load before the `enabled` store becomes globally visible. Only the two `pending_halt` accesses participate in the `SeqCst` total order `S`; the four `enabled` stores do not, and nothing relates `S` to the modification order of `enabled`. The bad interleaving is therefore admitted by both the abstract model and real hardware:

  1. session task loads `pending_halt` → `NO_HALT` (before the GUI's `fetch_or` in `S`);
  2. session task stores `enabled = true` (buffered);
  3. session task loads `pending_halt` again → still `NO_HALT`, satisfied ahead of the buffered store;
  4. GUI thread runs all three of its operations: `enabled = false`, `fetch_or(PlayerStopped)`, `enabled = false`;
  5. the session task's `enabled = true` drains **last**.

  Final state: `enabled == true` with `pending_halt == PlayerStopped`.
- **Why it matters here:** These really are two different OS threads. `request_halt(HaltSource::PlayerStopped)` runs on the eframe/egui main thread (`src/ui/mod.rs:253`, the shipped build's only Stop path — the doc there says it "never rides the bounded queue" precisely so saturation cannot suppress it), `request_halt(HaltSource::ActuatorFailed)` runs on the actuator task, and `gate.set(...)` runs from `apply()` on the session-loop task (`src/app/session/mod.rs:448`) on every snapshot, purchase, tick and command. So the window opens whenever a player presses Stop, or the actuator fails fatally, while a shop message is being dispatched. In that window `drop_reason` (`src/actuator/mod.rs:377`) reads `is_enabled() == true` and the executor keeps delivering clicks, and `capture_loop_budgeted` (`src/app/mod.rs:896`) keeps forwarding game traffic to the server — after the player asked it to stop. That breaks the documented contract of the function verbatim: "disables the gate *synchronously* and latches the cause so nothing can re-arm behind the player's back."

  Not P0 because it self-heals: the `biased` `halt_requested()` branch in `session_loop` (`src/app/session/mod.rs:81`) fires within one loop iteration, dispatches `Event::Stop`/`Event::ActuatorFailed`, and the resulting `apply()` sets the gate off for real. So the exposure is bounded by one select iteration rather than permanent, and there is no data loss. It is still a real race in the crate's only safety cutoff, and no test can catch it: all five `watch.rs` tests and `ui/mod.rs::full_command_queue_cannot_drop_stop` are single-threaded, where the interleaving cannot occur.
- **Fix:** Preferred — collapse the two variables into **one** atomic, which removes the cross-variable ordering problem instead of paying for it. One `AtomicU8` with the halt mask in bits 0..2 and `enabled` in bit 7: `request_halt` becomes a single `fetch_or(mask, Relaxed)` that also clears the enabled bit via `fetch_and`/CAS, `set(true)` becomes one CAS that refuses to set the enabled bit while any halt bit is set, both double-checks disappear, and `Relaxed` suffices throughout because there is only one location left to order.

  Minimal patch, if the two-field shape is kept: make every operation that participates in the handshake `SeqCst`, i.e. the four `enabled.store(...)` calls in `set` and `request_halt` (lines 74, 79, 83, 85, 99, 105) as well as the two `pending_halt` loads. The Dekker argument then closes: `store_sc(enabled,true) <S load_sc(pending_halt) <S fetch_or_sc(pending_halt) <S store_sc(enabled,false)`, so `false` is the last store to `enabled` in `S`. `is_enabled()` may stay `Acquire` (or drop to `Relaxed`) — readers only observe, and coherence already guarantees they converge on the final value.

  Either way, add a multi-threaded regression test: spawn a thread hammering `set(true)` in a loop against a thread calling `request_halt`, and assert `!(is_enabled() && pending_halt != NO_HALT)` after each round. It will fail on the current code on a loaded x86 box and is the only way this stays fixed.
- **Effort:** small (minimal patch) / medium (single-atomic redesign, which is the one worth doing)

### conc-002 — `SnapshotEpoch` uses `AcqRel`/`Acquire` for a counter that publishes nothing

- **Severity:** P3
- **Rule:** [`conc-atomic-ordering`](../../.claude/skills/rust-skills/rules/conc-atomic-ordering.md)
- **Site:** `src/actuator/mod.rs:32` (`fetch_add(1, Ordering::AcqRel)`), `src/actuator/mod.rs:37` (`load(Ordering::Acquire)`)
- **What:** `bump()` uses `AcqRel` on a `fetch_add` whose return value is discarded; `current()` uses `Acquire`.
- **Why it matters here:** The epoch is only ever *compared for equality* (`job.epoch != epoch.current()` at `src/actuator/mod.rs:375`). No memory is published through it: the snapshot itself reaches the controller under its own mutex, and the epoch value reaches the executor baked into a `Job` travelling over an `mpsc` channel, whose `send`/`recv` already creates the happens-before edge between `bump()` on the session task and the executor's read. The safety property ("a click planned against a dead shop must not land") is unaffected by the ordering: a weaker load is not a *staler* load — any load must read some value in the modification order, and `Acquire` adds no freshness guarantee that `Relaxed` lacks. So `Relaxed` on both is provably sufficient, and the `AcqRel` RMW is a needless barrier on every shop message.
- **Fix:** `fetch_add(1, Ordering::Relaxed)` and `load(Ordering::Relaxed)`. Keep the existing doc comment; add one line saying the channel, not the atomic, carries the ordering.
- **Effort:** trivial

### conc-003 — `PressureResync` uses `AcqRel`/`Acquire`/`Release` throughout for a flag whose payload rides an `mpsc` channel

- **Severity:** P3
- **Rule:** [`conc-atomic-ordering`](../../.claude/skills/rust-skills/rules/conc-atomic-ordering.md)
- **Site:** `src/app/mod.rs:73-131` — six operations: `compare_exchange(…, AcqRel, Acquire)` at 79-80, 97-98 and 110-111, `load(Acquire)` at 102 and 124, `store(RESYNC_ACK, Release)` at 117, `swap(RESYNC_ACK, AcqRel)` at 128
- **What:** Every access to the three-state `AtomicU8` (`ACK`/`PENDING`/`ENQUEUED`) carries acquire and/or release semantics.
- **Why it matters here:** The protocol is single-producer (the capture thread does `ACK→PENDING→ENQUEUED`) / single-consumer (the reassembly task does `ENQUEUED→ACK`), and the *only* thing published across the boundary is the `CaptureEvent::PressureResync` marker, which travels over the `mpsc::Sender<CaptureEvent>` (`src/app/mod.rs:104`) and gets its edge from the channel. Nothing else is written before a transition and read after it, so no acquire/release pairing is load-bearing. The correctness properties that *do* matter here — the marker is never enqueued twice, and never lost when `try_send` reports `Full` — rest on RMW atomicity and on the mo total order over RMWs on a single location, both of which `Relaxed` already provides. Eventual visibility (the capture thread must resume once the consumer acknowledges) is also guaranteed under `Relaxed`. `Relaxed` on all six is provably sufficient.
- **Fix:** `Relaxed` for both orderings of each `compare_exchange`, for the two loads, the store and the swap. Note in the type's doc comment *why* — this atomic is a state machine, not a publication channel — so the next reader does not "strengthen" it back.
- **Effort:** trivial

### conc-004 — pcap's `stop` and `capture_loss` flags carry `Release`/`Acquire`/`AcqRel` with no payload to publish

- **Severity:** P3
- **Rule:** [`conc-atomic-ordering`](../../.claude/skills/rust-skills/rules/conc-atomic-ordering.md)
- **Site:** `src/capture/pcap.rs:521` and `:621` (`stop.store(true, Release)`), `:922` (`stop.load(Acquire)`), `:1026` (`capture_loss.store(true, Release)`), `:674` (`capture_loss.swap(false, AcqRel)`)
- **What:** Two `AtomicBool` signals — the teardown flag shared with `PcapStop` and every capture thread, and the driver-drop flag — use `Release`/`Acquire`/`AcqRel`.
- **Why it matters here:** In both cases the boolean *is* the whole message. `stop` publishes no data: a capture thread that observes `true` closes **its own** `pcap_t` (the `Handle` it exclusively owns, dropped on that same thread per `src/capture/pcap.rs:410-440`) and drops its own sender — the module's design note at `:503-514` is explicit that no cross-thread handle operation happens, which is exactly why there is nothing for an `Acquire` to acquire. `capture_loss` publishes nothing either; the `PcapStat` counters it was derived from are thread-local `previous` state in `poll_drops`. `swap` must stay an RMW (the read-and-clear must be atomic), but its ordering can be `Relaxed`. So `Relaxed` is provably sufficient on all five sites.
- **Fix:** `Relaxed` on the three stores, the load and the swap. The comment at `:503-514` already contains the justification — reference it.
- **Effort:** trivial

### conc-005 — `EventLog::generation` uses `Release`/`Acquire` although the payload it announces is behind a `Mutex`

- **Severity:** P3
- **Rule:** [`conc-atomic-ordering`](../../.claude/skills/rust-skills/rules/conc-atomic-ordering.md)
- **Site:** `src/journal.rs:91` (`generation.fetch_add(1, Ordering::Release)`), `src/journal.rs:95` (`load(Ordering::Acquire)`)
- **What:** `push` deliberately does `drop(entries)` and *then* bumps the generation with a `Release` `fetch_add`; readers load it `Acquire`.
- **Why it matters here:** The ordering is written as if the generation published the entries, but it cannot: the only way to read the entries is `EventLog::entries()` (`src/journal.rs:100`), which takes the same `Mutex`, and the mutex unlock/lock pair is already the happens-before edge — a stronger one than the atomic provides. The generation is purely a change hint for the GUI cache at `src/ui/mod.rs:142-146`. A `Relaxed` load that returns a stale value costs one frame of a 4 Hz repaint, which is the same outcome `Acquire` gives (an `Acquire` load carries no freshness guarantee either). `Relaxed` on both is provably sufficient.
- **Fix:** `fetch_add(1, Ordering::Relaxed)` and `load(Ordering::Relaxed)`; keep `drop(entries)` before the bump (it is right for a different reason — not holding the lock longer than needed).
- **Effort:** trivial

### conc-006 — `pending_halt`'s two non-handshake accesses are `SeqCst` for no reason

- **Severity:** P3
- **Rule:** [`conc-atomic-ordering`](../../.claude/skills/rust-skills/rules/conc-atomic-ordering.md)
- **Site:** `src/watch.rs:113` (`load(SeqCst)` in `halt_requested`), `src/watch.rs:132` (`fetch_and(!mask, SeqCst)` in `acknowledge_halt`)
- **What:** Two of the four `pending_halt` accesses take no part in the store-load handshake of `conc-001` yet still pay for a full barrier.
- **Why it matters here:** `halt_requested` only needs to observe the mask; the value is self-describing and the wake-up edge is `Notify`'s, so `Relaxed` suffices (the `loop { check; notified().await }` shape is already correct against a lost wake-up because `Notify` retains a permit — the comment at `:117-118` says so). `acknowledge_halt` needs RMW atomicity so a concurrent `fetch_or` cannot be lost; RMWs on one location are totally ordered in mo regardless of `Ordering`, and in practice `acknowledge_halt` and the following `set(...)` are both on the session-loop task, ordered by program order. `Relaxed` suffices for both.
- **Fix:** `Relaxed` on both — **but only as part of `conc-001`**. Do not weaken these in isolation and do not weaken the two `pending_halt` accesses in `set`/`request_halt`, which must stay `SeqCst` under the minimal fix. If `conc-001` is fixed by collapsing to a single atomic, all of it becomes `Relaxed` naturally.
- **Effort:** trivial

### conc-007 — the shield thread's setup verdict travels in an `Arc<Mutex<Option<…>>>` that the readiness channel could carry itself

- **Severity:** P3
- **Rule:** [`conc-scoped-threads`](../../.claude/skills/rust-skills/rules/conc-scoped-threads.md)
- **Site:** `src/actuator/shield.rs:159-188` (`spawn_window`)
- **What:** `spawn_window` allocates `Arc<Mutex<Option<Result<isize, String>>>>`, clones it into the thread, and pairs it with an `mpsc::channel::<()>` that carries only a unit signal.
- **Why it matters here:** Honest scoping first: `std::thread::scope` itself does **not** apply — the pump thread must outlive the function (it owns the window for the process lifetime), so this is not the rule's literal violation. What does apply is the Bad-example pattern the rule is built on: an `Arc` clone existing purely to hand a value to a spawned thread, where a cheaper mechanism is available. Here `mpsc::channel::<Result<isize, String>>` carries the verdict directly: a successful `recv()` yields the detailed `create_window` error, and `Err(RecvError)` — the sender dropped without sending — is precisely "the shield thread died during setup", which is the fallback the current code already writes. The stated justification ("a detailed failure survives even when the signal never arrives") only holds for a thread that panics between `*lock(&thread_outcome) = Some(created)` and `tx.send(())`, two adjacent statements that cannot panic. So the `Arc`, the `Mutex`, the `Option`, and the `take()` are all removable, and one fewer lock exists in a module the actuator touches on every job.
- **Fix:** Change the channel to `std::sync::mpsc::channel::<Result<isize, String>>()`, `tx.send(created)` inside the thread (checking `is_ok()` on a clone or re-reading before the move for the `run` decision), and replace the tail with `rx.recv().unwrap_or_else(|_| Err("the shield thread died during setup".to_owned()))?`. Delete the `outcome`/`thread_outcome` pair.
- **Effort:** small

## Context (not findings, but worth recording)

- **Lock-ordering audit: no cycle exists.** The crate holds seven distinct locks — `SessionHandles::controller`, `EventLog::entries`, `ui::SessionErrorSlot`, `ActuatorHandle::timings`, `BudgetInner::usage`, `shield::WINDOW`, and `shield`'s local `outcome`. Exactly two nestings occur anywhere:
  - `controller` → `timings`, from `apply()` and its callees, which run while the controller guard is live (`src/app/session/mod.rs:254`, `:271`, `:280`, `:286`, `:336`, `:364`, `:408` reaching `actuator.timings()` at `:496`, `:577`, `:623` and `actuator.set_timings()` at `:280`). Every one of the four `timings` call sites in the crate is inside that subtree, so the pair is only ever taken in this order; the GUI never touches `timings` at all (it seeds the editor from the startup config). No inversion, no deadlock.
  - `shield::WINDOW` → `outcome`, from `handle()` → `spawn_window()`. The shield thread only ever takes `outcome`, never `WINDOW`, so again no cycle.

  Both `journal.emit(...)` calls in `session/mod.rs` are deliberately placed *after* `drop(ctrl)`, and `ui/mod.rs` scopes its controller and error guards to single statements, so `controller` and `entries` are never held together. This is worth stating because it is easy to break: adding a `journal.emit` inside `apply()` would create a `controller` → `entries` edge, and the GUI's cache path already reads `entries` — still no cycle today, but the invariant is unwritten.
- **`shield::WINDOW` is held across a thread spawn and a blocking `rx.recv()`** (`src/actuator/shield.rs:142-155` calling `:159-188`), so a concurrent `hide()` blocks for the length of window creation. Not a deadlock — `hide()` and `raise()` are both driven from the single actuator task — and not a rule violation, but it is the kind of "lock across a blocking handshake" that becomes one the moment a second caller appears. Fixing `conc-007` shortens the critical section but does not remove it.
- **`stream.rs`'s unwind path is safe by construction, and non-obviously so.** `try_retag` can panic with `usage` locked (`assert!` at `src/stream.rs:176`), and the unwind then drops `BudgetedSegment`s whose `PayloadLease::drop` re-locks the same non-reentrant `Mutex`. This does not deadlock only because the guard is a local of `try_retag`'s frame and is released (poisoning the mutex) before the caller's frames unwind, and because every lock site uses `unwrap_or_else(|err| err.into_inner())`. The `release` doc comment at `:182-191` shows this was reasoned about; the interaction with `try_retag`'s own `assert!` is the half not written down.

## Clean areas

- **`conc-thread-local` — fully clean.** There is no `static mut` in the crate, so the Rust 2024 `static_mut_refs` hard error is not merely avoided, it is unreachable. `src/crash.rs` was checked specifically: the panic hook keeps *no* global state at all — it closes over the previous hook in a `Box` handed to `set_hook`, and every helper (`crash_entry`, `crash_log_paths_from`, `write_first_writable`) is pure or takes its inputs as arguments, which is also what makes them testable. `src/actuator/win.rs` was checked specifically for Win32 global state: the only one is `static DPI: Once` (`:53`), and `Once::call_once` is exactly the right primitive for a process-global flip that must happen once; there is no window hook, no `SetWindowsHookEx`, and no thread-local ambient state. `src/actuator/shield.rs:27` uses `static WINDOW: Mutex<Option<isize>>` — a genuinely process-global handle cache, correctly guarded, with a documented poison policy (`:29-34`) and a documented reason it must never cache a failure. `thread_local!` is not used and is not needed: the two per-thread accumulators that exist (`Funnel` in `src/capture/pcap.rs:461`, and `delivered`/`unstrippable`/`dropped` in `capture_loop`) are plain locals owned by their thread, which is strictly better than a thread-local.
- **`conc-atomic-ordering` — the two `Relaxed` choices in the crate are the correct ones, in both directions.** `src/stream.rs:226`, `:233`, `:242`, `:255-257` — `dropped_segments`/`dropped_bytes`/`resyncs` are independent statistics counters read only for logging and the GUI, which is the rule's canonical `Relaxed` case; the `fetch_update` form is used for saturating arithmetic, not for ordering, and the comment at `:220-221` correctly explains why the discarded `Result` is not a swallowed error. `src/main.rs:283`/`:333` — the `failed` flag is `Relaxed` *and relied upon correctly*: the publication edge is the `session_task` join at `:318`, not the atomic, and on the timeout path the load can only read `false`, which is benign because the timeout is already reported by the `warn!` at `:320`. `src/config.rs:1017-1019` — a test-only monotonic id source, `Relaxed`, correct.
- **No `std` guard crosses an `.await`.** Verified in the two places it would matter: `session_loop`'s handlers all `drop(ctrl)` (or return, releasing the guard) before any `.await`, and the doc at `src/app/session/mod.rs:28-29` states the rule explicitly; `BudgetedChunk::retag_outbound` (`src/stream.rs:342-356`) takes the `usage` lock only inside the synchronous `try_retag` and holds nothing across `notified.await`.
- **The `Notify` handshake in `retag_outbound` is the correct lost-wakeup-free shape** — `notified()`, `pin!`, `.enable()` to register, *then* attempt the retag, *then* await — paired with `release()` doing `drop(usage)` before `notify_waiters()` (`src/stream.rs:208-209`). `notify_waiters` stores no permit, so registering before the check is not optional here; this is right and should not be "simplified".
- **`unsafe impl Send for Handle`** (`src/capture/pcap.rs:430`) is justified by ownership rather than by hope: the type is non-`Clone`, non-`Sync`, moved once into its capture thread, and closed by `Drop` on the thread that owns it — the comment at `:410-430` states the invariant that makes it sound, and the code upholds it.
- **Teardown ordering is thought through**: `SessionWorkers::shutdown` moves the parking `stop_and_join` onto `spawn_blocking` with a comment explaining that parking a runtime worker on a single-worker runtime is a deadlock (`src/app/mod.rs:513-524`), and `PcapStop` is a flag rather than a cross-thread `pcap_close`, with the comment recording that the alternative "burned this codebase once" (`src/capture/pcap.rs:503-514`).

## Not applicable

- **`conc-rayon-par-iter` — does not apply, and should not be made to.** `rayon` is not a direct dependency (`Cargo.toml` has no mention of it; it appears in `Cargo.lock` only transitively under the `egui_kittest`/`wgpu`/`image` dev-dependency tree), and there is no `par_iter`/`par_sort` anywhere in `src`, `examples` or `build.rs`. There is also no CPU-bound data-parallel workload to apply it to: the pipeline is IO-bound end to end (Npcap reads, TCP reassembly over `BTreeMap`/`VecDeque` of one flow, a `wss://` send, `PostMessageW` plus `thread::sleep` beats), and the only per-element loops — `Reassembler`'s gap walk, `render_match`'s slot scan over ≤ 6 shop slots, `LinkStrip::ip_bytes`' VLAN walk — are microseconds of work on collections of single-digit to low-hundreds length, where the rule's own guidance ("small collections: sequential is often faster due to thread-spawn overhead") says to stay sequential. Adding rayon here would add a thread pool, a dependency and build time to a single-file player exe in exchange for nothing.
- **`conc-scoped-threads` — structurally inapplicable at all three real spawn sites** (`conc-007` above is the one adjacent nit worth acting on). Every thread this crate starts must outlive the function that starts it, which is precisely what `thread::scope` forbids: the *n* pcap capture threads are stored as `Vec<JoinHandle<()>>` in the `PcapSource` returned from `open()` (`src/capture/pcap.rs:581-585`, joined in `Drop` at `:620-627`); the `capture` thread's handle is stored in the `CaptureWorker` returned from `spawn_capture_with_budget` (`src/app/mod.rs:580-594`, joined in `stop_and_join`); the `shield` thread runs `GetMessageW` forever (`src/actuator/shield.rs:167`). Nor would scoping remove any `Arc`: the values shared with those threads (`Arc<AtomicBool>` stop/loss flags, `WatchGate`, `EventLog`, `PipelineBudget`, `PressureResync`) are `Arc`-backed *by type* because the same handles are shared with the GUI thread and stored in long-lived structs, not because a stack borrow was avoided. `examples/ui_preview.rs:138` is the same story: its detached poll thread moves `Arc<Mutex<Controller>>`, `WatchGate` and `EventLog` clones, all of which `SessionHandles` requires as owned `'static` values anyway, so a scope around `run_native` would save no allocation — it would only convert a detached thread into a joined one, and the thread already exits within 40 ms of the channel disconnecting.
