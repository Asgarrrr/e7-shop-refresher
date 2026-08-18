# 17 — Closures (`closure-`)

**Category priority:** MEDIUM
**Rules audited:** 5 · **Files read:** 40 (+ `Cargo.toml`) · **Findings:** 3 (P0 0 / P1 0 / P2 0 / P3 3)

## Verdict

This category is close to clean, and not by accident: the crate is on edition 2024
(so disjoint capture is active), every one of its six declared `Fn`-family bounds is
already the weakest that works, and the `clone`-before-`move` idiom is applied
correctly at every one of the eleven thread/task boundaries. The two `widget_info(||
…)` closures in `src/ui/theme.rs` and `src/ui/journal.rs` are *exemplary* — they bind
`enabled` to a local and capture only `Copy` values, with a comment explaining why the
string is built inside the closure. What is left is three redundant-work nits, all P3:
two clones that a `move` closure does not need (`src/main.rs:307`, clippy-confirmed;
`src/capture/pcap.rs:577`, verified by compiling the pattern) and four test closures
that reach for `RefCell` although the harness bound is already `FnMut`. The single
highest-value fix is `src/main.rs:307` — one deleted `.clone()`, and it is the one site
clippy will keep reporting.

Two scope notes. First, the `Box<dyn Fn>`-vs-`impl Fn` question the brief anticipated
does not arise here: **no function in this crate returns a closure**, and the only
closure stored in a struct field is a test double (`FakeSurface::on_input`), where
`Box<dyn FnMut>` is the correct choice per `closure-static-vs-dyn`'s own decision
table. The `Box<dyn Trait>` values that *do* exist (`WinSurface::driver`,
`CaptureSource::packets`/`stop`, `CaptureWorker::stop`) are ordinary trait objects, not
closures — they belong to `anti-type-erasure`, and I have deliberately left them there.
Second, `cargo clippy -W clippy::redundant_closure_for_method_calls` flags five
closures (`src/crash.rs:22`, `src/stream.rs:147/168/193/248`), four of them
`unwrap_or_else(|err| err.into_inner())` where the rest of the crate writes
`unwrap_or_else(std::sync::PoisonError::into_inner)`. That is a real inconsistency, but
none of the five `closure-` rule files states it, so I am recording it here as evidence
for whoever owns the clippy/style pass rather than filing it as a violation of mine.

## Findings

### closure-001 — `config_path` is cloned into a `FnOnce` closure that could just own it

- **Severity:** P3
- **Rule:** [`closure-move-capture`](../../.claude/skills/rust-skills/rules/closure-move-capture.md)
- **Site:** `src/main.rs:307`
- **What:** inside the eframe app-creator closure,
  ```rust
  Box::new(move |cc| {
      Ok(Box::new(ui::ShopApp::new(cc, handles, error, seed_timings, config_path.clone())))
  }),
  ```
  `handles` and `error` are moved in, but `config_path` is cloned. The clone buys
  nothing: `eframe::AppCreator<'app>` is
  `Box<dyn 'app + FnOnce(&CreationContext<'_>) -> …>` (verified in
  `eframe-0.35.0/src/epi.rs:49`), the closure runs at most once, and `config_path` is
  never read again after `run_native` — `clippy::redundant_clone` confirms it with
  *"this value is dropped without further use"*.
- **Why it matters here:** the rule's whole point — `move` transfers ownership, so clone
  only when you need the value in *both* places — is already honoured for the two
  neighbouring arguments on the same line, which makes the third read as an oversight
  rather than a decision. The cost is one `PathBuf` allocation at startup, so this is a
  consistency and reviewer-confidence fix, not a performance one. `examples/ui_preview.rs:180`
  builds the identical closure and moves its `PathBuf` in without cloning, so the two
  spellings of the same call already disagree.
- **Fix:** drop `.clone()`:
  ```rust
  Ok(Box::new(ui::ShopApp::new(cc, handles, error, seed_timings, config_path)))
  ```
- **Effort:** trivial

### closure-002 — a `String` field is cloned to name a thread the closure then consumes whole

- **Severity:** P3
- **Rule:** [`closure-disjoint-capture`](../../.claude/skills/rust-skills/rules/closure-disjoint-capture.md)
- **Site:** `src/capture/pcap.rs:577` (the `let device = handle.device.clone();` in
  `PcapSource::open`'s per-adapter spawn loop)
- **What:**
  ```rust
  for handle in handles {
      let device = handle.device.clone();
      let sender = sender.clone();
      let stop = stop.clone();
      let capture_loss = capture_loss.clone();
      let thread = std::thread::Builder::new()
          .name(format!("pcap-{}", short_device_name(&device)))
          .spawn(move || capture_loop(handle, &sender, &stop, &capture_loss))
  ```
  The three `Arc`/`Sender` clones below it are genuine — each thread needs its own owned
  handle. `device` is not: it is used *only* in the `.name(…)` argument, which is fully
  evaluated before the closure literal is constructed, so `&handle.device` borrows for
  the length of the `format!` and is released before `handle` is moved. I compiled the
  reduced pattern (owning struct, borrow in `.name()`, whole struct moved into
  `.spawn()`) on edition 2024: it builds.
- **Why it matters here:** this is the mirror image of the footgun the rule describes.
  The rule tells you to bind a field to a local so the rest of the struct stays usable
  past a `move`; here the local was created for a use that happens *before* the move, so
  the borrow was always legal and the clone is pure ceremony. It reads as "the borrow
  checker made me do this", which is the wrong lesson to leave in a file this heavily
  annotated. Cost is one `String` per adapter, once, at open time — negligible.
- **Fix:** delete the binding and inline the borrow:
  ```rust
  .name(format!("pcap-{}", short_device_name(&handle.device)))
  ```
- **Effort:** trivial

### closure-003 — four test closures use `RefCell` although the harness bound is `FnMut`

- **Severity:** P3
- **Rule:** [`closure-fn-trait-bounds`](../../.claude/skills/rust-skills/rules/closure-fn-trait-bounds.md)
- **Site:** `src/ui/statusbar.rs:257` and `:282`; `src/ui/journal.rs:186`;
  `src/ui/editor/mod.rs:807` (plus the three `use std::cell::RefCell;` at
  `src/ui/statusbar.rs:161`, `src/ui/journal.rs:158`, `src/ui/editor/mod.rs:790`)
- **What:** each site smuggles a value out of a kittest render closure through interior
  mutability, e.g.
  ```rust
  let clicked = RefCell::new(None);
  let mut harness = Harness::new_ui(|ui| {
      if let Some(command) = render_status_bar(ui, &view, None, true) {
          *clicked.borrow_mut() = Some(command);
      }
  });
  harness.get_by_label("Start").click();
  harness.run();
  drop(harness);
  assert_eq!(clicked.into_inner(), Some(Command::Start));
  ```
  The bound does not require it: `Harness::new_ui` is
  `pub fn new_ui(app: impl FnMut(&mut egui::Ui) + 'a)`
  (`egui_kittest-0.35.0/src/lib.rs:866`, stored as `Box<dyn FnMut(&mut egui::Ui) + 'a>`),
  so the closure may capture `&mut clicked` directly. Every site already does
  `drop(harness)` before reading the value, which is exactly what releases the mutable
  borrow.
- **Why it matters here:** the brief asked specifically for callers forced into interior
  mutability by an over-tight `Fn` bound, because that is a high-value P1/P2 signal. I
  looked, and **this crate has no such case** — the bound is `FnMut`, so the `RefCell`
  is opportunistic, not forced. That is why this is a P3 and not a P2: it costs a
  runtime borrow check and an import in test code, nothing more. It is worth recording
  because `src/ui/editor/mod.rs:807` is the version a reader is most likely to copy when
  writing the next kittest, and `src/ui/journal.rs:207` already shows the borrow-free
  style for the cases that need no output.
- **Fix:** capture mutably and read after the harness is dropped, e.g.
  ```rust
  let mut clicked = None;
  let mut harness = Harness::new_ui(|ui| {
      if let Some(command) = render_status_bar(ui, &view, None, true) {
          clicked = Some(command);
      }
  });
  harness.get_by_label("Start").click();
  harness.run();
  drop(harness);
  assert_eq!(clicked, Some(Command::Start));
  ```
  In `run_setup` (`src/ui/editor/mod.rs:806`) the closure already captures `editor`
  mutably, so adding a second `&mut` capture changes nothing structurally. Drop the
  three `use std::cell::RefCell;` lines with the last site.
- **Effort:** small

## Clean areas

**`closure-fn-trait-bounds` — all six declared bounds are already minimal.** Audited
one by one, and there is nothing to tighten or loosen:

- `src/actuator/mod.rs:190` `fn blocking<T>(call: impl FnOnce() -> T)` — called exactly
  once, in one of two mutually exclusive match arms. `FnOnce` is the weakest possible
  bound and the only one that accepts the consuming closures at
  `src/actuator/mod.rs:214/222/229/286`.
- `src/ui/mod.rs:374` `content_inset<R>(ui, add_contents: impl FnOnce(&mut Ui) -> R)` —
  forwarded straight to `Frame::show`, which is itself `FnOnce`. Generic over `R` on
  purpose, so `&dyn Fn` is not an option.
- `src/ui/editor/mod.rs:387` `limit_ledger_row(…, value: impl FnOnce(bool, &mut Ui))` and
  `:416` `compact_drag(ui, add: impl FnOnce(&mut Ui) -> Response)` — each invoked once
  per call; both callers pass closures that mutate a captured `&mut Option<T>`, which
  `FnOnce` accepts and `Fn` would have rejected.
- `src/uplink/websocket.rs:100` `C: FnMut(String) -> F` — the connector is called once
  per reconnect attempt inside `loop`, so `FnOnce` is impossible and `Fn` would be
  gratuitously strict on the two test connectors.
- `src/actuator/mod.rs:457` `on_input: Box<dyn FnMut() + Send>` — `FnMut` although all
  three closures assigned to it (`:468`, `:638`, `:669`, `:1098`) would satisfy `Fn`.
  That is the *correct* direction: `FnMut ⊇ Fn`, so the field accepts more.

**`closure-move-capture` — clone-before-`move` is applied correctly at every boundary.**
`src/app/mod.rs:583` (`let panic_fatal = fatal.clone();` before the capture thread takes
`fatal`), `src/app/mod.rs:494` (`let fatal = fatal.clone();` before `tokio::spawn`, with
the `&'static str` name captured by copy), `src/actuator/shield.rs:165`
(`Arc::clone(&outcome)` before the pump thread takes it), `src/main.rs:274`
(`let (slot, flag) = (error.clone(), failed.clone());` — narrowed to precisely the two
fields the task touches, not the whole `SessionHandles`),
`src/capture/pcap.rs:578-580`, `src/uplink/websocket.rs:352/417`,
`src/app/mod.rs:1969-1996`, `examples/ui_preview.rs:128-138`.

**`closure-disjoint-capture` — no closure captures more than it uses.** The classic
finding the brief warned about (capturing a whole `Arc<Session>` to touch one field)
does not occur: `src/main.rs:278` moves in exactly `session`, `gate`, `flag`, `slot`,
and `examples/ui_preview.rs:138` exactly `receiver`, `controller`, `gate`, `journal`.
Three sites deserve to be pointed at as the reference implementations:

- `src/app/mod.rs:518-519` — `let mut capture = self.capture;` binds the field to a local
  *before* `spawn_blocking(move || capture.stop_and_join())`, so `self.tasks` stays usable
  afterwards. This is verbatim the rule's "bind to a local to narrow the capture".
- `src/ui/theme.rs:218-225` and `src/ui/journal.rs:54-61` — `let enabled = ui.is_enabled();`
  hoisted out so the `widget_info` closure captures only `Copy` values (`&str`,
  `Option<&str>`, `bool`) and never `ui`, with a comment stating the intent. Do not
  "simplify" the string construction out of these closures: egui only calls them when
  AccessKit or a test harness is live, which is the entire point.
- `src/app/session/mod.rs:49` — `let now_ms = || journal.now_ms();` borrows the journal
  by shared reference and is called from eight branches; no `move`, no clone.

**No closure is boxed where a generic would do.** `src/crash.rs:15`'s
`Box::new(move |info| …)` is mandated by `std::panic::set_hook`, whose parameter is
`Box<dyn Fn(&PanicHookInfo) + Sync + Send + 'static>` — `Fn` is also correct there, the
hook fires many times from many threads. `src/actuator/mod.rs:457` is a struct field
holding heterogeneous closures, which the `closure-static-vs-dyn` decision table sends
to `Box<dyn Fn>` explicitly.

## Not applicable

- [`closure-impl-fn-return`](../../.claude/skills/rust-skills/rules/closure-impl-fn-return.md)
  — no function in the crate returns a closure, boxed or otherwise (`grep` for
  `-> impl Fn`, `-> Box<dyn Fn` over `src/`, `examples/`, `build.rs`: zero hits). Nothing
  to convert, and nothing at risk of being converted the wrong way.
- [`closure-static-vs-dyn`](../../.claude/skills/rust-skills/rules/closure-static-vs-dyn.md)
  — partially applicable and already satisfied (see Clean areas). There is no hot inner
  loop taking `&dyn Fn`, no closure registry, and no generic-over-`F` struct field. The
  crate's `Box<dyn Trait>` fields hold *traits*, not closures, so the code-size trade-off
  this rule arbitrates is decided by `anti-type-erasure`, not here.
- The `Box<dyn Fn>` vs `impl Fn` overlap the brief flagged: covered from the
  closure-specific side only, as instructed. `WinSurface::driver`
  (`src/actuator/win.rs:143`), `CaptureSource::packets`/`stop`
  (`src/capture/mod.rs:25-26`) and `CaptureWorker::stop` (`src/app/mod.rs:446`) are left
  to the anti-patterns reviewer.
