# 11 — Trait & Generics Design (`trait-`)

**Category priority:** MEDIUM
**Rules audited:** 6 · **Files read:** 42 (all 41 `.rs` files + `Cargo.toml`) · **Findings:** 2 (P0 0 / P1 0 / P2 1 / P3 1)

## Verdict

This crate defines **four** traits, and every one of them exists for a reason
that survives inspection: `PacketSource` + `CaptureStop` (`src/capture/mod.rs`)
are the capture seam, `Surface` (`src/actuator/mod.rs`) is the input-backend
seam, `InputDriver` (`src/actuator/win.rs`) is the Win32-syscall seam that lets
the event-order tests prove validation happens before injection. None is
speculative, none is a "Factory of Factories", and all four are dyn-compatible.
The required-vs-defaulted split is deliberate and documented in the two traits
that have one. `cargo clippy --all-targets` is silent across the whole crate, so
nothing here is lint-visible.

What is left is a single genuine question, asked at two sites that answer it in
**opposite** directions: `WinSurface` erases a driver that is concrete in every
shipped build (`Box<dyn InputDriver>`, static dispatch would do), while the
actuator backend — the one thing here that really *is* selected at runtime from
config — is dispatched statically and pays for it with a duplicated
six-argument `run_executor` spawn per backend. The higher-value fix is the
second one (`trait-002`): it deletes a 24-line copy-paste in `Session::run` and
lets the `Send` requirement that `src/actuator/win.rs:565` documents in prose
become a supertrait the compiler checks. Worst offender file, on this one axis
only: `src/app/mod.rs`.

## Findings

### trait-001 — the actuator backend is chosen at runtime yet dispatched statically, duplicating the executor spawn

- **Severity:** P2
- **Rule:** [`trait-dyn-vs-generic`](../../.claude/skills/rust-skills/rules/trait-dyn-vs-generic.md)
- **Site:** `src/app/mod.rs:368-400` (the `match config.actuator.backend` block); trait at `src/actuator/mod.rs:150`, consumed at `src/actuator/mod.rs:270` (`run_executor(mut surface: impl Surface, …)`)
- **What:** `run_executor` takes `impl Surface`, so the two backends cannot share
  a call site. The result is two arms whose bodies are identical except for the
  first argument:

  ```rust
  ActuatorBackend::Input => workers.spawn(
      "actuator", &fatal_tx,
      run_executor(WinSurface::default(), job_rx, gate.clone(),
                   actuator.epoch.clone(), journal.clone(), dry_run),
  ),
  ActuatorBackend::Message => workers.spawn(
      "actuator", &fatal_tx,
      run_executor(MessageSurface::default(), job_rx, gate.clone(),
                   actuator.epoch.clone(), journal.clone(), dry_run),
  ),
  ```

  The rule's decision table puts this case squarely on the `dyn` side —
  *"Plug-in / callback registered at runtime → `dyn Trait`"*: `ActuatorBackend`
  is read from `config.toml` (`src/config.rs:139-149`), so which `Surface` runs
  is not known until startup.
- **Why it matters here:** Three concrete costs, all paid for a dispatch saving
  that cannot be observed. (1) Maintenance: `run_executor` has six parameters
  and every future one must be added twice, identically — `job_rx` is moved into
  whichever arm runs, so the arms cannot be factored without the trait object.
  (2) Binary size: `run_executor` is a large `async fn` (a hand-rolled state
  machine over a `while let` + inner `for` + three `await` points) monomorphised
  twice into a single-file exe. (3) The dispatch saving is nil by construction:
  every `Surface` method is called through `blocking()` and each one sleeps
  30–170 ms of Win32 settle/hold beats (`MOVE_SETTLE_MS`, `FOCUS_SETTLE_MS`,
  `SHIELD_DRAIN_MS`, `press_ms`) — a vtable indirection is unmeasurable next to
  a `std::thread::sleep`, which is exactly the "binary size matters, inlining
  does not" row of the table.
- **Fix:** Take the surface erased, and give `Surface` the `Send` supertrait its
  use already requires:

  ```rust
  // src/actuator/mod.rs
  pub trait Surface: Send { … }               // already dyn-compatible as-is
  pub async fn run_executor(mut surface: Box<dyn Surface>, …)

  // src/app/mod.rs — one spawn, no duplicated argument list
  let surface: Box<dyn Surface> = match config.actuator.backend {
      ActuatorBackend::Input => Box::new(WinSurface::default()),
      ActuatorBackend::Message => Box::new(MessageSurface::default()),
  };
  workers.spawn("actuator", &fatal_tx,
      run_executor(surface, job_rx, gate.clone(),
                   actuator.epoch.clone(), journal.clone(), dry_run));
  ```

  `SurfaceJobGuard<'a, S: Surface>` and its `Drop` impl keep working unchanged
  against `S = Box<dyn Surface>` only if `Box<dyn Surface>` itself implements
  `Surface`; the simpler route is to relax the guard to
  `SurfaceJobGuard<'a>(Option<&'a mut dyn Surface>)`, which also drops one type
  parameter. The `Send` supertrait is the part worth keeping either way: today
  the requirement is enforced only structurally at the `tokio::spawn` site and
  explained in a prose comment (`src/actuator/win.rs:565-567`, "the handle is
  stored as an integer so the executor's future stays `Send` across awaits") —
  as a supertrait, a future backend that stores a raw `HWND` fails at its own
  `impl`, next to the mistake, instead of at a spawn in another module.
- **Effort:** small

### trait-002 — `WinSurface` boxes a driver that is concrete in every shipped build

- **Severity:** P3
- **Rule:** [`trait-dyn-vs-generic`](../../.claude/skills/rust-skills/rules/trait-dyn-vs-generic.md)
- **Site:** `src/actuator/win.rs:143` (`driver: Box<dyn InputDriver>`), constructed at `:150` and `:158`
- **What:** `InputDriver` has exactly two implementors: `SystemInputDriver`
  (`:95`) and `#[cfg(test)] FakeInputDriver` (`:809`). Production therefore only
  ever holds `Box::new(SystemInputDriver)` — a zero-sized type behind a heap
  pointer and a vtable. The erasure buys nothing: `WinSurface` is not stored in a
  heterogeneous collection, is not returned from a trait method, and no second
  real driver exists.
- **Why it matters here:** This is the "single known concrete type" row of the
  decision table, and the shape it produces is slightly worse than the generic
  one in two ways beyond the (negligible) indirection: `with_driver` needs a
  `+ 'static` bound it would not otherwise need, and the seven `self.driver.…`
  calls in `validate_target` / `ensure_foreground` / `release_after_down` are
  opaque to the optimiser in a module whose whole point is a precise, ordered
  sequence of syscalls. Severity is P3 because nothing is *wrong*: one
  allocation per session, and the indirection is again dwarfed by the driver's
  own `sleep`.
- **Fix:** A generic parameter with a default type — no call site changes:

  ```rust
  pub struct WinSurface<D: InputDriver = SystemInputDriver> {
      driver: D,
      target: Option<Target>,
  }

  impl Default for WinSurface { /* driver: SystemInputDriver */ }

  #[cfg(test)]
  impl<D: InputDriver> WinSurface<D> {
      fn with_driver(driver: D) -> Self { Self { driver, target: None } }
  }

  impl<D: InputDriver> Surface for WinSurface<D> { … }
  ```

  `WinSurface::default()` in `src/app/mod.rs:379` still resolves through the
  default type parameter, and `fake_surface()` (`:893`) infers
  `WinSurface<FakeInputDriver>`. Note this is compatible with `trait-001`: the
  `dyn` boundary belongs at `Surface` (runtime-selected), not at `InputDriver`
  (compile-time-selected) — the two findings pull in opposite directions on
  purpose, because the two seams sit on opposite sides of the decision table.
- **Effort:** trivial

## Clean areas

**`trait-dyn-vs-generic` — dispatch chosen correctly**

- `CaptureSource { packets: Box<dyn PacketSource>, stop: Box<dyn CaptureStop> }`
  (`src/capture/mod.rs:24-27`) is the right side of the table: the pair is
  *stored across calls*, moved wholesale into an OS thread
  (`src/app/mod.rs:580-590`), and there are 5 `PacketSource` / 3 `CaptureStop`
  implementors. Making it generic would push two type parameters through
  `CaptureSource`, `spawn_capture_with_budget`, `CaptureWorker` and
  `SessionWorkers` for one virtual call per captured packet, on a path whose
  cost is `Receiver::recv` + `parse_segment`.
- `src/uplink/websocket.rs:92-102` is the textbook static-dispatch case:
  `run_with_connector<C, F, S>` with `S: Stream<Item = …> + Sink<Message, Error = WsError> + Unpin`,
  monomorphised over the real `connect_async` stream in production and over
  `StalledLink` in tests. No boxing anywhere in the uplink.
- `impl Trait` in argument position is used consistently where the type is known
  at the call site: `blocking(call: impl FnOnce() -> T)`
  (`src/actuator/mod.rs:190`), `CaptureSource::new(packets: impl PacketSource + 'static, …)`
  (`src/capture/mod.rs:31-34`), `SessionWorkers::spawn(future: impl Future<Output = ()> + Send + 'static)`
  (`src/app/mod.rs:492`), `supervise(session: impl Future<…>)` (`:623`),
  `input_loop(input: impl AsyncBufRead + Unpin)` (`:1034`),
  `Config::load(path: impl AsRef<Path>)` (`src/config.rs:326`),
  `persist::save` / `strip_retired_keys` (`src/config/persist.rs:51`, `:156`),
  `content_inset(add_contents: impl FnOnce(&mut egui::Ui) -> R)`
  (`src/ui/mod.rs:374`), `limit_ledger_row(value: impl FnOnce(bool, &mut egui::Ui))`
  and `compact_drag(add: impl FnOnce(…) -> Response)`
  (`src/ui/editor/mod.rs:388`, `:416`), and `styled(row, text: impl Into<String>)`
  (`src/ui/shop.rs:95`) — the last with a comment explaining why it is a free
  generic function rather than a closure, which is precisely the reasoning this
  rule asks for.
- The two `dyn` uses that are *not* a design choice are correctly left alone:
  `panic_message(payload: &(dyn Any + Send))` (`src/crash.rs:38`) has its
  signature dictated by `PanicHookInfo::payload()`, and
  `FakeSurface::on_input: Box<dyn FnMut() + Send>` (`src/actuator/mod.rs:457`)
  stores a closure in a struct field, which is the one thing `impl Fn` cannot do.

**`trait-object-safety` — all four traits are dyn-compatible, and the ones that need it say why**

- No trait in the crate has a generic method, an associated const, or a `-> Self`
  by value, so none of the four could ever fail at a `dyn` use site.
- The `Send` supertraits are load-bearing and minimal:
  `PacketSource: Send` / `CaptureStop: Send` (`src/capture/mod.rs:46`, `:87`)
  because both boxes are moved into `std::thread::Builder::spawn`;
  `InputDriver: Send` (`src/actuator/win.rs:76`) because `WinSurface` must stay
  `Send` for `tokio::spawn`. Nothing carries a `Sync` bound it does not use.
- `CaptureStop`'s doc comment (`src/capture/mod.rs:42-45`) states the two
  contract clauses a vtable cannot encode — idempotence, and "must not close a
  raw OS handle concurrently with receive" — and `PcapStop` (`:515-524`) is
  built as an `AtomicBool` store *specifically* to satisfy them, with the
  regression that motivated it recorded in the type's own docs. That is the right
  way to carry an invariant a trait object cannot.

**`trait-default-methods` — the required/defaulted split is minimal in both traits that have one**

- `PacketSource` (`src/capture/mod.rs:87-102`): one required method
  (`next_segment`), one defaulted (`take_capture_loss` → `false`), with the doc
  comment naming exactly who keeps the default ("Backends that cannot lose
  packets keep the default"). `PcapSource` overrides it because Npcap's
  `ps_drop` can move; three of the four test fakes keep it.
- `Surface` (`src/actuator/mod.rs:150-170`): three required
  (`acquire`/`click`/`scroll`), one defaulted (`release` → no-op) whose contract
  ("must be idempotent and non-panicking because it runs from a destructor") is
  stated on the trait. The empty default is safe rather than a trap here because
  cleanup is belt-and-braces: `SurfaceJobGuard::release_once`
  (`src/actuator/mod.rs:225-232`) always calls it, and `MessageSurface` *also*
  has a `Drop` impl (`src/actuator/win.rs:587-591`) that hides the shield.
- `InputDriver`'s seven required methods with no defaults is correct, not lazy:
  each one is a distinct Win32 entry point the fake must intercept and record
  (including `sleep`, which the fake logs instead of performing — a default body
  would silently make the ordering tests sleep for real).
- `CaptureStop` has a single method; there is nothing to build a default on.

**`trait-coherence-newtype` — the orphan rule is respected everywhere it is touched**

- Every foreign-trait impl in the crate targets a *local* type:
  `Deref for BudgetedChunk` / `for BudgetedSegment` (`src/stream.rs:378`, `:403`),
  `PartialEq<Vec<u8>> for BudgetedChunk` (`:372`, test-only),
  `Debug for BudgetedChunk` (`:365`), `Drop` on `PayloadLease`/`Handle`/
  `PcapSource`/`MessageSurface`/`LiveGuard`/`TempDir`,
  `Default` on `Config`/`ReconnectConfig`/`EventLog`/`WinSurface`/`FakeState`,
  `eframe::App for FatalApp` / `for ShopApp` (`src/ui/mod.rs:64`, `:132`),
  `Stream`/`Sink<Message> for StalledLink` (`src/uplink/websocket.rs:274`, `:281`).
- The `#[repr(transparent)]` newtypes the rule's "See Also" points at are not
  needed: the FFI structs (`PcapIf`, `PcapPktHdr`, `BpfProgram`, `PcapStat`) are
  `#[repr(C)]` layout mirrors, not trait-impl wrappers.

**`trait-blanket-impl`**

- No blanket impl exists and none is missing: there is no class of types
  receiving repeated identical behaviour. The five `PacketSource` implementors
  have five materially different `next_segment` bodies (blocking condvar,
  one-shot, immediate error, gate-flipping, loss-reporting); the same holds for
  the three `CaptureStop`s.

**`trait-associated-type-vs-generic`**

- No trait in this crate has an associated type or a generic parameter, and
  none needs one — every trait has exactly one implementor-independent output
  (`Segment`, `ClientRect`, `()`), all named concretely. Where the crate *does*
  need to pin somebody else's associated type it does it the way the rule
  prescribes, in a bound rather than as a free parameter:
  `S: Stream<Item = Result<Message, WsError>> + Sink<Message, Error = WsError>`
  (`src/uplink/websocket.rs:102`, `:178`) and `Deref { type Target = [u8]; }`
  (`src/stream.rs:379`).

## Not applicable

- **`trait-blanket-impl`** — no repeated identical behaviour across a class of
  types, and the crate is `publish = false`, so the rule's semver caution has no
  bite either. Adding a `impl<T: PacketSource + ?Sized> PacketSource for Box<T>`
  forwarding impl (the `impl<R: Read + ?Sized> Read for Box<R>` pattern) *would*
  let `capture_loop_budgeted` take `impl PacketSource` and drop `Box::new(…)`
  from four test call sites — deliberately **not** filed, because that is
  inventing an abstraction to save four words.
- **`trait-associated-type-vs-generic`** — no trait here is generic over an
  input type or produces a per-implementor output type; the choice the rule
  arbitrates never comes up.
- **`trait-coherence-newtype`** — the crate never wants a foreign trait on a
  foreign type. The nearest thing, the `wide(&str) -> Vec<u16>` helper duplicated
  at `src/actuator/win.rs:45` and `src/capture/pcap.rs:890`, is a free function
  on a foreign type, which the orphan rule permits outright; it needs no newtype
  (whether it wants an extension trait is `api-extension-trait`'s call, and the
  duplication itself belongs to a sibling reviewer).

## One pointer outside this category

`CaptureStop::stop(&mut self) -> Result<()>` (`src/capture/mod.rs:47`) is
infallible in all three implementors — `PcapStop` is an `AtomicBool::store` that
always returns `Ok(())` — which leaves a permanently dead error branch at
`src/app/mod.rs:454-456`. That is a `type-result-fallible` / `err-` question
about the return type, not a trait-shape question, so it is flagged here only so
the synthesis does not lose it.
