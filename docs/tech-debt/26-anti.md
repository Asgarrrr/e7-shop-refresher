# 26 — Anti-patterns (`anti-`)

**Category priority:** REFERENCE
**Rules audited:** 15 · **Files read:** 39 (38 `.rs` under `src/` + `examples/ui_preview.rs`, plus `build.rs` and `Cargo.toml`) · **Findings:** 10 (P0 0 / P1 3 / P2 4 / P3 3)

## Verdict

This crate does not have an anti-pattern problem; it has a *diagnostic-silence*
problem in three specific places and a *duplicate-invariant* problem in one
module. There is **not a single `.unwrap()` in non-test code** anywhere in the
crate (all 253 sites are inside `#[cfg(test)]`), no `&String`/`&Vec<T>`
parameter, no manual `for i in 0..len` indexing, and no lock guard held across
an `.await`. Of the 26 `let _ =` sites I judged individually, **23 are
deliberately correct** and most carry the reason on the line above them — the
exhaustive table is below, and it is the strongest evidence in this report that
the code was written by someone who already thinks about this rule.

The three that are not correct all share one shape: a swallowed error in an app
whose *only* diagnostic channel is a log file. The worst offender file is
`src/main.rs`, which drops the first-run config seed error (anti-003) and the
log-appender error (anti-004) — the second one being the log system failing
silently about its own failure. **The single highest-value fix is anti-001**:
`actuator::win::ensure_dpi_awareness` discards the one return value that tells
you whether the entire click-coordinate pipeline is valid, and in the shipped
GUI build that call *always* fails, so the discard is total.

Every finding below is something `cargo clippy --all-targets` cannot see — I ran
it and confirmed it is completely silent on this crate.

## Findings

### anti-001 — The DPI-awareness result is discarded, and in the shipped build it is always an error

- **Severity:** P1
- **Rule:** [`anti-empty-catch`](../../.claude/skills/rust-skills/rules/anti-empty-catch.md) (sibling: the `err-` reviewer's error-propagation rules)
- **Site:** `src/actuator/win.rs:52-63`
- **What:** the Win32 return is dropped entirely — not even `let _ =`, just a
  trailing semicolon:

  ```rust
  DPI.call_once(|| {
      // SAFETY: ...
      unsafe {
          SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
      }
  });
  ```

  The doc comment defends it: *"winit already sets this in gui builds; the
  console build needs it here — a failed call means it was already set, which is
  what we want."* That conflates **"already set"** with **"already set to what we
  want."** `SetProcessDpiAwarenessContext` returns `FALSE` with
  `ERROR_ACCESS_DENIED` for *any* already-set awareness, including
  `UNAWARE` and `SYSTEM_AWARE`.
- **Why it matters here:** this is not a cosmetic call. The whole coordinate
  chain assumes physical pixels: `client_rect()` (`win.rs:301`) reads
  `GetClientRect` + `ClientToScreen`, `plan::to_screen` scales design-space
  points against that rect, and `move_cursor` normalizes the result against
  `SM_CXVIRTUALSCREEN` — which is *always* physical. If the process is
  DPI-unaware or system-aware, `GetClientRect` returns virtualized coordinates
  on a per-monitor-scaled display, every planned point is off by the scale
  factor, and clicks land on the wrong buttons.

  And the failure is silent by construction: `SendInput` is documented not to
  report UIPI blocking (`win.rs:510-515`) and reports success here too, while
  `PostMessageW` posts a perfectly valid message to a wrong coordinate. Nothing
  in the funnel, the journal or `crash.log` would say a word.

  Reachable how: any per-exe *"Override high DPI scaling behavior"* compatibility
  setting, `__COMPAT_LAYER=HighDpiAware`, an AppCompat shim, or a future
  eframe/winit that picks V1 or system-aware. Worse, in the **shipped GUI build
  this branch already always fails** — eframe/winit sets the process awareness
  before the actuator's first `acquire()` ever runs the `Once` — so the app's
  correctness rests entirely on winit's undocumented choice, verified nowhere,
  logged nowhere.
- **Fix:** check the return, and read back what the process actually has:

  ```rust
  DPI.call_once(|| {
      // SAFETY: unchanged.
      let set = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
      if set == 0 {
          // Already set — by winit in the GUI build, or by a compatibility
          // shim. Which one decides whether GetClientRect is in physical pixels.
          // SAFETY: both calls take no pointer and only report process state.
          let ctx = unsafe { GetThreadDpiAwarenessContext() };
          let aware = unsafe { GetAwarenessFromDpiAwarenessContext(ctx) };
          tracing::info!(
              awareness = aware,
              error = %std::io::Error::last_os_error(),
              "process DPI awareness was already set; click coordinates assume physical pixels"
          );
      }
  });
  ```

  Ideally the actuator then refuses (`SurfaceError::Fatal`) on anything but
  per-monitor awareness, since a mis-aimed click is worse than no click — but
  the log line alone converts an unreproducible bug report into a one-line
  answer. Needs `Win32_UI_HiDpi`'s `GetThreadDpiAwarenessContext` /
  `GetAwarenessFromDpiAwarenessContext`, already in the enabled feature.
- **Effort:** small

### anti-002 — Wire tolerance in `domain::shop` discards the deserialization error with no log line

- **Severity:** P1
- **Rule:** [`anti-empty-catch`](../../.claude/skills/rust-skills/rules/anti-empty-catch.md) (sibling: `serde-` for the tolerant-deserializer shape itself)
- **Site:** `src/domain/shop.rs:49` (`object_or_none`) and `src/domain/shop.rs:64` (`lenient_elements`)
- **What:** `Ok(serde_json::from_value::<T>(value).ok())` and
  `.filter_map(|value| serde_json::from_value(value).ok())`. The *policy* is
  right and documented ("a partial, `null`, or mistyped value degrades to `None`
  rather than failing the whole snapshot"). The error itself is thrown away —
  nothing counts it, nothing logs it.
- **Why it matters here:** these two functions decode
  `ShopItem::limit`, `ShopSnapshot::refresh` and `ShopItem::substats`, and both
  degradations change what the app *does*, not just what it shows:
  - a mistyped or partial `limit` degrades to `None`, so `ShopItem::is_sold_out`
    returns `false` (`shop.rs:104`), `Controller::plan_targets` marks the slot
    `in_reach` (`control/mod.rs:536`), the actuator clicks Buy on a sold-out
    slot, no purchase echo ever arrives, and the watchdog climbs the whole
    ladder to `StopReason::Unresponsive` — a halt that blames the game;
  - a partial `refresh` degrades to `None`, which silently disables
    `StopReason::OutOfFunds` (`control/mod.rs:678-682`), so the loop keeps
    issuing paid refreshes against a balance it can no longer see.

  Both are exactly the "unreproducible bug report" the brief describes: the
  player sends the log file and it contains no trace that a field was ever
  rejected. The crate's own test suite proves the degradation paths exist
  (`refresh_partial_object_degrades_to_none`, `partial_limit_degrades_to_buyable`)
  — so this is a known behaviour with no observability.
- **Fix:** keep the tolerance, add the line. Both functions are in a crate that
  already depends on `tracing`, and neither is in a hot path (once per shop
  message):

  ```rust
  fn object_or_none<'de, D, T>(de: D) -> Result<Option<T>, D::Error> /* ... */ {
      let value = serde_json::Value::deserialize(de)?;
      match serde_json::from_value::<T>(value) {
          Ok(parsed) => Ok(Some(parsed)),
          Err(err) => {
              tracing::debug!(error = %err, "tolerated an undecodable side-channel object");
              Ok(None)
          }
      }
  }
  ```

  `debug!` is enough: the default filter is
  `arkyve_refresh_shop=debug,journal=info,warn` (`main.rs:92`), so it lands in
  the file. A player-visible journal line would be over-reporting.
- **Effort:** trivial

### anti-003 — First-run config seeding fails silently, then blames a file that does not exist

- **Severity:** P1
- **Rule:** [`anti-empty-catch`](../../.claude/skills/rust-skills/rules/anti-empty-catch.md)
- **Site:** `src/main.rs:44` and `src/main.rs:46`
- **What:**

  ```rust
  if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
      let _ = std::fs::create_dir_all(parent);
  }
  let _ = std::fs::write(path, EXAMPLE);
  ```

  The doc comment says "Best-effort: any failure is ignored and `Config::load`
  falls back to the in-memory defaults, exactly as before." That is a correct
  description of the *control flow* and a false description of the *outcome*.
- **Why it matters here:** the in-memory defaults are **not** equivalent to the
  seeded example. `Config::default()`'s filter is unrestricted
  (`config.rs:1195` asserts exactly that), and the relay refuses to hunt on an
  unrestricted filter. So a failed seed produces:
  - console build: an immediate hard exit with
    `"no [filter] criteria in config.toml — define what to hunt (see config.example.toml)"`
    (`app/mod.rs:294`) — naming a file that was never written;
  - GUI build: a window that boots, refuses arming, and shows
    `"Idle — define a filter first"` while the Setup tab's Apply writes to a
    path whose parent `create_dir_all` just failed on.

  And this is one of the few `let _ =` sites where logging is *available*:
  `install_logging()` runs at `main.rs:125`, fifteen lines before
  `seed_config_if_missing` at `main.rs:140`. The ordering was clearly thought
  about (`migrate` deliberately runs before it, and `leftovers.report()` after)
  — this call just missed the memo. Realistic triggers: a redirected or
  read-only `%APPDATA%`, a roaming-profile quota, an antivirus lock, or a
  managed machine.
- **Fix:**

  ```rust
  if let Err(err) = std::fs::write(path, EXAMPLE) {
      tracing::warn!(
          path = %path.display(), error = %err,
          "could not seed config.toml; starting on built-in defaults, which set no hunt filter"
      );
  }
  ```

  Same for `create_dir_all` (or let the `write` failure report both). Two lines,
  and it turns the console build's misleading message into a two-line diagnosis
  in the file the player is asked to send.
- **Effort:** trivial

### anti-004 — The log system swallows the reason it could not open its own log

- **Severity:** P2
- **Rule:** [`anti-empty-catch`](../../.claude/skills/rust-skills/rules/anti-empty-catch.md) (sibling: `obs-`)
- **Site:** `src/main.rs:83`
- **What:** `Err(_) => (BoxMakeWriter::new(std::io::stdout), None)` — the
  `tracing_appender::rolling::Builder::build(log_dir())` error is discarded by
  the pattern itself. The comment explains the *fallback* ("fall back to stdout
  rather than to no subscriber at all — inert in the windowed build, real in the
  console one") and correctly notes that in the shipped windowed build that
  fallback writes to nothing.
- **Why it matters here:** this is the one swallowed error in the crate whose
  loss is *self-concealing*. The windowed build has no console
  (`main.rs:6`), so "fall back to stdout" means "produce no output at all". A
  player whose `%LOCALAPPDATA%\arkyve-refresh-shop\logs` is unwritable gets an
  app with **no log file and no explanation anywhere** — and that state is not
  hypothetical: it is precisely the condition `src/migrate.rs` exists to repair
  (the admins-only protected DACL a WinDivert build left behind), plus the
  ordinary OneDrive/AV/quota cases. If `migrate` fails to reset the DACL it
  *does* warn (`migrate.rs:100-105`) — but that warning is emitted through the
  subscriber this very line just failed to install, so both halves of the
  diagnosis vanish together.

  The crate already contains the right pattern for this, twelve lines up:
  `migrate::Leftovers` exists *exactly* to carry findings across the
  no-subscriber window (`migrate.rs:38-52`, "the ordering is the whole reason
  this is a value rather than a set of `warn!` calls in place").
- **Fix:** two parts, both cheap.
  1. Try the temp dir before falling back to an inert writer — `crash.rs`
     already establishes that ladder (`crash_log_paths_from`: app-data first,
     temp dir as "a guaranteed-writable fallback"). A log in `%TEMP%` is worth
     far more than no log.
  2. Return the error alongside the guard and report it once the subscriber
     exists, the `Leftovers` way:

     ```rust
     fn install_logging() -> (Option<WorkerGuard>, Option<(PathBuf, std::io::Error)>) { /* ... */ }
     // in main, after .init():
     if let Some((dir, err)) = log_fallback {
         tracing::warn!(dir = %dir.display(), error = %err, "log directory unusable; using the fallback");
     }
     ```
- **Effort:** small

### anti-005 — The "never leave the left button held" invariant is implemented twice, independently

- **Severity:** P2
- **Rule:** [`anti-over-abstraction`](../../.claude/skills/rust-skills/rules/anti-over-abstraction.md) (the inverse: copy-paste that should have been abstracted)
- **Site:** `src/actuator/win.rs:221-240` (`WinSurface::release_after_down`) and `src/actuator/win.rs:660-674` (inline inside `MessageSurface::click`). Same module: `win.rs:167` / `:577` (identical `target()` bodies), `win.rs:198-207` / `:619-633` (identical minimized/moved rect classification).
- **What:** the two input backends duplicate three things rather than share them:

  | Behaviour | `WinSurface` | `MessageSurface` |
  |---|---|---|
  | "post the release, retry once, report the worse fault" | `release_after_down`, 20 lines | inline in `click`, 13 lines |
  | fatal message when both releases fail | `"…after two failed LEFTUP attempts"` | `"…after two failed WM_LBUTTONUP posts"` |
  | no-acquire guard | `"input attempted without an acquired game window"` | *the same literal, retyped* |
  | rect-change classification | `"…was minimized mid-job"` / `"…moved or resized mid-job"` | *the same two literals, retyped* |

  Six duplicated string literals and two independent implementations of a
  three-state retry decision whose logic the code itself flags as subtle
  (`"The retry's own verdict decides which error is told: same rule as
  WinSurface::release_after_down"` — a comment that only exists because the rule
  is not in one place).
- **Why it matters here:** the duplicated invariant is the safety-critical one.
  Leaving the game holding a synthetic left button is the single worst state
  this actuator can produce, and the guarantee now lives in two functions that a
  fix must be applied to twice. The tests do not protect against the drift:
  `two_failed_cleanup_left_ups_are_fatal` matches on
  `"could not be proven released"` — the common prefix — while
  `refused_guarded_left_up_is_retried_and_returns_original_error` covers only
  the `WinSurface` path. A third retry, a changed precedence, or a new
  `SurfaceError` variant added to one and not the other compiles and passes.
- **Fix:** one shared helper for each of the three:

  ```rust
  /// Always attempt the release; retry once; report the worse fault.
  fn release_twice(
      revalidate: impl FnOnce() -> Result<(), SurfaceError>,
      mut release: impl FnMut() -> Result<(), SurfaceError>,
      what: &str, // "LEFTUP" / "WM_LBUTTONUP"
  ) -> Result<(), SurfaceError> { /* the one implementation */ }

  const NO_TARGET: &str = "input attempted without an acquired game window";
  fn rect_change_error(observed: ClientRect) -> SurfaceError { /* minimized vs moved */ }
  ```

  The two `target()` bodies then collapse to
  `self.target.ok_or_else(|| SurfaceError::Fatal(NO_TARGET.to_owned()))`.
- **Effort:** small

### anti-006 — Seven `.expect()` calls on mutexes the rest of the crate deliberately treats as poisonable

- **Severity:** P2
- **Rule:** [`anti-panic-expected`](../../.claude/skills/rust-skills/rules/anti-panic-expected.md) · also [`anti-unwrap-abuse`](../../.claude/skills/rust-skills/rules/anti-unwrap-abuse.md) and [`anti-expect-lazy`](../../.claude/skills/rust-skills/rules/anti-expect-lazy.md) (sibling: `err-`)
- **Site:** `src/app/session/mod.rs:200, 237, 326, 357, 406` (`.expect("controller mutex poisoned")`) and `src/actuator/mod.rs:97, 105` (`.expect("actuator timings mutex poisoned")`)
- **What:** this crate has an explicit, written house rule that a poisoned lock
  must not cascade, and five sites that honour it with the reason stated:

  | Site | Treatment |
  |---|---|
  | `journal.rs:77-80` | `unwrap_or_else(PoisonError::into_inner)` — *"panicking here after one poisoning would cascade across tasks and freeze the very history the GUI is meant to still show"* |
  | `ui/mod.rs:40-44` | `lock_ignoring_poison`, its own named helper |
  | `actuator/shield.rs:32-34` | `lock()` helper — *"Poisoning carries no meaning here"* |
  | `stream.rs:147, 168, 193, 248` | `unwrap_or_else(|err| err.into_inner())` |
  | `main.rs:288-290` | *"Poison-tolerant like the view's own reads … panicking here would kill this task silently"* |

  The seven sites above are the exception, and none states a reason. They guard
  the **same `Arc<Mutex<Controller>>` the GUI reads through
  `lock_ignoring_poison`** — so the two owners of one mutex disagree about what
  poisoning means.
- **Why it matters here:** I want to be honest about reachability, because it is
  the deciding factor. Poisoning the controller mutex is **not** currently
  reachable: `ShopApp::ui` copies the whole frame's state under a short lock and
  drops the guard before any egui call (`ui/mod.rs:138-141`), so a widget panic
  cannot poison it, and `Controller::handle` is pure and saturating throughout.
  The one indirect path is real but exotic: `apply()` calls
  `actuator.timings()` *while holding the controller guard*
  (`session/mod.rs:254`, via `queue_refresh`), so a poisoned **timings** mutex
  panics inside the controller's critical section and poisons that one too,
  after which every `dispatch`/`on_command`/`on_message` panics and the session
  dies for good.

  So this is a robustness and consistency defect, not a live bug — but it is
  the wrong default in a windowed no-console app, it is invisible to a reader
  who learned the house rule from the five documented sites, and it costs one
  line to remove. (Mitigating: `supervise` catches the panic and puts
  `"session crashed: …"` in the banner, and `crash.rs` records it — so it would
  not be a *silent* disappearance. That is why this is P2 and not P1.)
- **Fix:** one shared helper, placed next to the domain rather than the view, and
  the seven call sites switched to it:

  ```rust
  // in `crate::journal` or a small `sync` module
  pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
      mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
  }
  ```

  `ui::lock_ignoring_poison` and `shield::lock` are then the same function, which
  removes a third and fourth copy of the idiom as a bonus.
- **Effort:** trivial

### anti-007 — Three near-identical "optional numeric with an arming checkbox" widgets

- **Severity:** P2
- **Rule:** [`anti-over-abstraction`](../../.claude/skills/rust-skills/rules/anti-over-abstraction.md) (the inverse)
- **Site:** `src/ui/editor/mod.rs:325-349` (`limit_row`), `:354-376` (`duration_row`), `:769-786` (`optional_value`)
- **What:** all three implement the same semantics — unchecked means "no
  constraint", checking seeds a non-zero value via `get_or_insert(seed)`,
  unchecking writes `None`, and the `DragValue` uses
  `.range(1..=MAX).clamp_existing_to_range(false)` so a config-seeded `0`
  survives the first render. Only the surrounding chrome differs (grid cell vs
  ledger row vs ledger row with a minute↔ms conversion). The chrome *was*
  factored out (`limit_ledger_row`); the semantics were not.

  The duplication is visible in the comments: `limit_row`'s doc says *"Mirrors
  `optional_value`'s semantics — unchecked means 'no constraint', a freshly
  checked box seeds a non-zero value, and `clamp_existing_to_range(false)` keeps
  a config-seeded `0` from being silently rewritten to `1`"*, which is
  `optional_value`'s own doc comment paraphrased.
- **Why it matters here:** `clamp_existing_to_range(false)` is not a style
  choice — it is a regression fix with a named test
  (`seeded_zero_limit_is_not_silently_clamped`, `:1063`), because without it
  Apply sent `max_refreshes = 1` for a player who had written `0`. That test
  drives `edit_setup`, which reaches `limit_row`/`duration_row` but **not**
  `optional_value` (Hunt's `min substats` / `max price` grid). So one of the
  three copies of the fix is untested, and the next person to touch the widget
  has three places to keep in step.
- **Fix:** extract the value half, leave the three chrome layouts:

  ```rust
  /// The arming semantics, once: `None` is "no constraint"; a fresh arm seeds
  /// `seed`; an existing value is never silently clamped.
  fn optional_numeric<T: egui::emath::Numeric>(
      ui: &mut egui::Ui, armed: bool, value: &mut Option<T>, seed: T,
  ) { /* the one implementation */ }
  ```

  `duration_row` keeps only its minute conversion around the call.
- **Effort:** small

### anti-008 — A generic parameter added to save one `to_owned` per table cell, at 4 Hz, beside an uncached `format_item`

- **Severity:** P3
- **Rule:** [`anti-premature-optimize`](../../.claude/skills/rust-skills/rules/anti-premature-optimize.md) (sibling: `perf-profile-first`)
- **Site:** `src/ui/shop.rs:83-104` (`styled`), against `src/ui/view.rs:58-75` (`view_state`)
- **What:** `styled` carries a ten-line doc comment justifying its
  `impl Into<String>` parameter:

  > *"Takes `impl Into<String>` exactly like `RichText::new`, so each caller pays
  > one copy at most … A free function rather than a closure: a closure cannot be
  > generic over its argument, which is what forced the old `String`-only
  > signature — and with it a `to_owned` on a `&'static str` and a `clone` of a
  > name the frame never mutates."*

  The saving is one `String` allocation per table cell. The table is at most six
  rows × four columns, repainted at 4 Hz (`ui/mod.rs:137`) — call it ~100
  allocations per second, with no benchmark anywhere in the crate.
- **Why it matters here:** the same frame, in the same code path, rebuilds
  `SlotRow::detail` for every row by calling `render::format_item`
  (`view.rs:71`) — a function that performs up to six nested `format!` calls and
  a `Vec<String>` + `join` for the substats (`render.rs:130-161`) — with **no
  caching at all**, while the journal right beside it *is* generation-cached
  precisely to avoid per-frame re-cloning (`ui/mod.rs:91-93, 142-146`). So the
  optimization sits at the cheap end of the frame and skips the expensive one,
  in a module that demonstrably knows how to cache. That is the textbook shape
  of the rule: cleverness spent where it was not measured, absent where it
  would have paid.

  Nothing here is actually slow, and I am not proposing a caching layer for a
  six-row table. The cost being paid is clarity: a generic function plus a
  paragraph of justification, for a benefit the same file's own caching
  precedent implies is negligible.
- **Fix:** either (a) drop the generic — `fn styled(row: &SlotRow, text: impl Into<egui::RichText>)`
  or plain `&str` — and delete the paragraph, or (b) keep it and cut the comment
  to one line, so a reader does not conclude it was measured. If per-frame
  allocation in this tab ever *does* matter, the fix is to key `ViewState` on a
  snapshot generation the way `journal_cache` is keyed, not to shave the cells.
- **Effort:** trivial

### anti-009 — `push_str(&format!(…))` six times per shop item, in the one string the GUI rebuilds every frame

- **Severity:** P3
- **Rule:** [`anti-format-hot-path`](../../.claude/skills/rust-skills/rules/anti-format-hot-path.md) (sibling: `mem-write-over-format`)
- **Site:** `src/render.rs:130-161` (`format_item`)
- **What:** six occurrences of `line.push_str(&format!(" · {…}"))`, each
  allocating a throwaway `String` only to copy it into `line` and drop it, plus
  a `Vec<String>` + `join` for the substats.
- **Why it matters here:** `format_item` has two callers, and the second is the
  one that matters: `view_state` calls it once per shop row on **every repaint**
  (`view.rs:71`), i.e. ~6 rows × ~8 allocations × 4 Hz ≈ 200 throwaway `String`s
  per second in the shipped GUI build, for a value the frame usually re-derives
  identically. I want to be plain: **this is not a hot path** and it is not
  costing the player anything measurable. It is filed because it is the crate's
  one genuine instance of the rule's exact Bad example, the fix is mechanical,
  and clippy cannot see it — `clippy::format_push_string` is a
  restriction-group lint, off by default, which is part of why the crate is
  clippy-clean.
- **Fix:** `use std::fmt::Write;` and `write!(line, " · {name}").expect("String write is infallible")`
  — or, since `expect` in a formatting path is noise, `let _ = write!(…)` with the
  standard "writing to a `String` cannot fail" note. Pre-size with
  `String::with_capacity` if touching it anyway.
- **Effort:** trivial

### anti-010 — The one panic before the window bypasses `fatal()`, which exists for exactly this

- **Severity:** P3
- **Rule:** [`anti-panic-expected`](../../.claude/skills/rust-skills/rules/anti-panic-expected.md) · [`anti-expect-lazy`](../../.claude/skills/rust-skills/rules/anti-expect-lazy.md)
- **Site:** `src/main.rs:120-122`
- **What:**

  ```rust
  rustls::crypto::ring::default_provider()
      .install_default()
      .expect("install the rustls ring CryptoProvider");
  ```
- **Why it matters here:** the condition is genuinely a programming invariant —
  `install_default` fails only if a provider is already installed, and nothing
  else in this binary installs one — so by the letter of `anti-expect-lazy` an
  `.expect()` is defensible. What makes it worth a line is the *asymmetry with
  its neighbours*: the two other startup failures on the same page (invalid
  config, runtime that will not build) both route through `fatal()`
  (`main.rs:224-234`), which logs, prints to stderr, **and opens an error window
  in the windowed build** — because, as its own doc comment says, "a
  double-clicked exe must not flash a console and vanish". This path does none
  of that: it panics, `crash.rs` records it (the hook is installed five lines
  earlier, which is good), the default hook writes to an inert stderr, and the
  player sees the exe do nothing at all.
- **Fix:** two lines, reusing the machinery that is already there:

  ```rust
  if rustls::crypto::ring::default_provider().install_default().is_err() {
      return fatal("Failed to initialise TLS: a crypto provider was already installed.".to_owned());
  }
  ```
- **Effort:** trivial

## `anti-empty-catch` — exhaustive audit

The brief asks for a verdict on every silently swallowed error. All 26 `let _`
sites, all 3 `.ok()` sites, and every empty match arm in `src/`, judged
individually. **23 of 26 `let _ =` are deliberately correct**; the holes are the
three filed above.

| Site | Verdict |
|---|---|
| `main.rs:44, 46` | **HOLE** → anti-003. Logging is already installed at this point. |
| `main.rs:83` (`Err(_) =>`) | **HOLE** → anti-004. The one swallow that conceals itself. |
| `crash.rs:100` | Correct. `create_dir_all` inside the panic hook; documented ("the panic hook must never itself panic, and the open below reports real failures"). |
| `crash.rs:159, 167, 168` | Correct. Test fixture cleanup. |
| `config/persist.rs:94` | Correct. Removes the sibling temp after a failed atomic write; documented, and the real error is already being returned. |
| `config.rs:1037` | Correct. `TempDir::drop` (test), and better than the alternative it calls out. |
| `stream.rs:223, 230, 239` | Correct, and pre-empted: *"The discarded `fetch_update` results below are not swallowed errors: the closures always return `Some`."* True. Avoidable, though — a plain `fetch_add(1, Relaxed)` on a `u64` drop counter needs ~1.8e19 events to wrap, so the saturation, the closures, the three `let _ =` and the two-line comment defending them all buy nothing. Batchable P3 cleanup, not a defect. |
| `app/mod.rs:497` | Correct. `fatal.send` from the task panic-catcher; a closed receiver means the session loop already ended and will report its own outcome. |
| `app/mod.rs:588` | Correct. Same, `blocking_send` from the capture thread. |
| `app/mod.rs:666, 676` | Correct — and not an error at all: `flush_anchor` returns `bool` ("downstream still open"), and both sites are breaking out of the loop regardless. |
| `app/mod.rs:884` | Correct. The `error!("capture interrupted")` immediately above already logged it; the `let _` is on the *notification*, and it is guarded by `!*shutdown.borrow()`. |
| `app/mod.rs:2001, 2040`, `session/tests.rs:110` | Correct. Tests. |
| `actuator/shield.rs:173` | Correct. `tx.send(())` readiness handshake; the receiver is alive by construction (the spawner blocks on `rx.recv()`), and the *verdict* travels in the shared slot precisely so a lost signal loses nothing. |
| `actuator/shield.rs:184` | Correct and documented. *"Either the slot has been filled or the sender died with the thread; both end the wait, and the slot tells which."* |
| `actuator/win.rs:119` | Correct and documented at length. *"`SetForegroundWindow` reports FALSE both when the switch is denied and when it merely has not happened yet, so its verdict is worthless"* — and `ensure_foreground` re-reads the real foreground as the authority. |
| `actuator/win.rs:59-61` | **HOLE** → anti-001. Not even `let _ =`; the `BOOL` is dropped by a trailing semicolon. |
| `uplink/websocket.rs:114, 122, 132, 218` | Correct. `inbound.send(...)` where a closed receiver means `session_loop` has exited; the loop's own `None => { uplink_closed = true; break }` path owns that case. Undocumented at the lines — one shared comment would help. |
| `capture/ip.rs:21` (`.ok()?`) | Correct. A truncated or non-IP frame is *expected* traffic, and it is not silent: `PcapSource::next_segment` counts it into `Funnel::unparsed` and `Funnel::report` emits it on the first packet and every 500th. This is what a good version of anti-002 looks like. |
| `domain/shop.rs:49, 64` (`.ok()`) | **HOLE** → anti-002. Same tolerance as `ip.rs:21`, without the counter or the log. |
| `uplink/websocket.rs:220` | Correct. `Err(err) => debug!(error = %err, "unrecognized server message, ignored")` — the error is logged, and the default filter enables crate `debug`. |
| `app/mod.rs:536` (`Err(_) => break`) | Correct. Shutdown-grace timeout; the abort-and-join pass right after is the handler. |
| `app/mod.rs:1062` (`Ok(None) \| Err(_) => break`) | Minor hole, console build only. A genuine stdin read error (as opposed to EOF) kills the only input channel with no line anywhere. In the shipped windowed build stdin is inert, so this *is* the normal path. Not worth a finding; worth one `debug!` if the file is touched. |
| `actuator/win.rs:229` (`Err(_) => error`) | Correct, and deliberate: the *original* fault is the one reported, and the retry's own failure escalates to a distinct `Fatal`. |
| `session/mod.rs:317` (`Ack \| Unknown => {}`) | Correct and documented ("acks and unknown messages are silent"). |
| `session/mod.rs:519` (`Ok(()) => {}`) | Correct. The two `Err` arms both journal, with two different messages, on purpose (`SubmitError`). |
| `migrate.rs:106, 117` (`Ok(false) => {}`, `Err(NotFound) => {}`) | Correct. "Nothing to clean" is the steady state, and the module is explicitly built to be silent there. |
| `websocket.rs:189, 204` | Correct. `Ok(Ok(())) => {}` is success; `Some(Ok(_)) => {}` is commented ("ping/pong/frame: handled by the library"). |
| `app/mod.rs:710-790` (`ForwardStatus::Open => {}`) | Not errors — control flow over a three-state enum, exhaustively matched. |
| `ui/mod.rs:229-236` | Correct, and exemplary: the `persist::save` failure is journaled with the error, and the comment explains why it is non-fatal (the live retune already went through). |

## Systemic patterns

Five habits that recur across the whole codebase, worst first, and one that is
genuinely excellent.

**1. Deliberate silence, with three gaps in the only channel that exists.**
Every swallowed error in this crate is a decision, and 23 of 26 are the right
one — most with the reasoning written on the line above. The three exceptions
share a shape: they are the swallows that happen *before or around* the log
system (`main.rs`: the config seed, the appender itself) or *inside a pure
function that has no logger by design* (`domain/shop.rs`). This is not
carelessness, it is a blind spot with a boundary you can draw: wherever the
crate has a `tracing` call available it uses one; wherever it does not, it stops
thinking. Modules: `src/main.rs`, `src/domain/shop.rs`, `src/actuator/win.rs`.
The cure for all three is under ten lines total, and `migrate::Leftovers`
already demonstrates the hardest case (deferred reporting across a
no-subscriber window).

**2. A house rule with five documented adopters and seven silent defectors.**
Poison tolerance is stated as policy five times, in five different files, each
time with a reason — and then contradicted seven times with no reason at all,
on the *same mutex* the GUI reads tolerantly. This is the general failure mode
of conventions carried in prose: the five sites that explain themselves are the
ones that were touched during the change that established the rule, and the two
files that were not touched never learned it. Modules: tolerant in
`journal.rs`, `ui/mod.rs`, `actuator/shield.rs`, `stream.rs`, `main.rs`;
intolerant in `app/session/mod.rs`, `actuator/mod.rs`. One shared helper
function would make the rule unforgettable — and would incidentally delete two
existing copies of the same three-line idiom.

**3. "These two must behave identically" is enforced by comment, never by
code.** Wherever this crate has two implementations of one behaviour, it writes
a comment asserting they agree and then maintains them separately. The two
actuator backends duplicate a safety invariant (left-button release), a guard
message, and a rect-change classifier — six duplicated string literals between
them. The Setup editor has three copies of one arming semantic, and the
regression test for the subtle part covers two of the three. The `ui` widget
code repeats the "allocate an exact-size rect, `interact`, `widget_info`, paint
by hand" pattern four times (`theme::collapsing_section`,
`ui/journal::render_journal_header`, `ui/editor::limit_ledger_row`,
`timing_meter::timing_row`) with no shared scaffold. Notably, the crate *does*
factor aggressively when the shared thing is a value or a policy
(`persist::replace_file`, `render::describe`, `HAUL_HEADLINERS`,
`plan::WAIT_*`, `Timings::named_ranges`'s exhaustive destructuring) — it just
does not factor *behaviour across two backends*. Modules:
`src/actuator/win.rs`, `src/ui/editor/mod.rs`, `src/ui/theme.rs`.

**4. Micro-optimization applied at the cheap end of the frame, absent at the
expensive one.** The UI layer contains three carefully-argued allocation
optimizations — a generic parameter to avoid one `to_owned` per table cell, two
`widget_info` closures built lazily so a `format!` is not paid per frame, and a
generation-gated journal cache — none of them measured, and one of them
(`ui/shop.rs::styled`) sitting directly beside an uncached `format_item` that
runs six nested `format!`s per row on the same frame. There is no benchmark
anywhere in the crate (`Cargo.toml` has no `[[bench]]`, no criterion), which is
the right call for a 4 Hz six-row table — and exactly why the three
optimizations should not be there either. Modules: `src/ui/shop.rs`,
`src/ui/view.rs`, `src/ui/journal.rs`, `src/ui/theme.rs`, `src/render.rs`.

**5. Player-facing state is a formatted `String`, and the tests assert on
prose.** Every decision the domain makes reaches the player as a pre-rendered
line (`">> actuator: {reason} — dropped planned clicks"`), and roughly forty
tests across `app/session/tests.rs`, `actuator/mod.rs` and `actuator/win.rs`
assert `line.text.contains("…")` on those sentences. It works, and the wording
is genuinely good — but it means every message is load-bearing test surface, and
a reworded line is a red test suite rather than a rendering change. `render.rs`
already shows the fix direction (`StopReason` → `describe`, `RefusalReason` →
`refusal`): the domain hands over an enum, one module turns it into words. That
discipline stops at the domain boundary; the actuator and the session loop build
their strings inline. I did **not** file this — it is the `type-` reviewer's
`anti-stringly-typed` territory and it is a design trade, not a defect — but it
is the habit most likely to make a future change feel expensive.

### What this codebase does exceptionally well

It would be dishonest to end on faults. Concretely, and by category:

- **Zero `.unwrap()` in production code.** All 253 sites are inside
  `#[cfg(test)]`, verified file by file. Almost no crate of this size can say
  that, and it is not an accident: `err-`-style handling is threaded all the way
  through, including a `Result` for a coordinate transform
  (`plan::to_screen`) and a `#[must_use]` on `ActuatorHandle::submit` whose
  message is *"a rejected job means a lost click — journal the drop"*.
- **`Drop` is treated as an abort hazard.** `stream.rs:182-210` degrades an
  accounting underflow to `error!` + `debug_assert!` specifically because
  `PayloadLease::drop` can run while a worker unwinds, and a panic there would
  abort the process with no `crash.log` — with a `#[cfg(not(debug_assertions))]`
  test pinning the saturating behaviour and a `#[cfg(debug_assertions)]`
  `should_panic` twin pinning the fail-fast. That is a level of care most crates
  never reach.
- **Fail-open vs fail-closed is stated, not implied.** `filter.rs:11-15`
  declares the asymmetry ("`max_price` is fail-closed … sold-out is fail-open")
  and the tests pin both halves. Same for the gold estimate
  (`unknown_gold_restricts_nothing`) and the dedup fingerprint
  (`fail_open_snapshot_keeps_last_identity`).
- **Two errors that look alike are kept apart on purpose.** `SubmitError`
  splits `QueueFull` from `ExecutorGone` with the note *"that mistake already
  cost one full investigation"*; `Error::ConfigReparse` is kept distinct from
  `ConfigSerialize` because *"flattened to one string the two were
  indistinguishable in the banner, though only one of them is the player's
  fault"*; `post_refusal` and `preflight_refusal` split UIPI from a dead window
  because the two need opposite advice. This is error-design maturity.
- **Win32 last-error ordering is right everywhere.** Every `GetLastError` read
  is placed before the next Win32 call, with a comment saying why — including
  the awkward case in `migrate::dacl_is_protected` where it must be read before
  `LocalFree`.
- **Saturating arithmetic is used deliberately and tested at the edges.**
  `DelayRange::draw`'s `checked_add` on the modulus, `slack_from_target`'s
  `f32::clamp` totality (with a `const _: ()` tripwire *and* the runtime guard,
  because "the two guards protect different edits"), `Haul::record`,
  `effective_slot`'s `u8::try_from(...).unwrap_or(u8::MAX)`. Each has a test
  named after the failure it prevents.
- **Size canaries on the per-packet types.** `capture/mod.rs:77-83` and
  `stream.rs:418-424` assert `size_of` on the values that ride a 512-slot
  channel, with the correct caveat that these are not ABI contracts and a
  failure means "re-measure deliberately".
- **The comments are load-bearing and honest.** Several carry a `⚠ Untested`
  marker (`ethernet_payload_offset`), several say what was *measured* and on
  what machine, and several document a rejected alternative
  (`build.rs`'s `uiAccess='true'`, `migrate`'s null-DACL trap). The brief warned
  me not to propose deleting rationale comments; having read all 39 files, that
  warning was correct and I have proposed deleting exactly one paragraph
  (anti-008), for the one comment that argues for something unmeasured.

## Not applicable

- **`anti-stringly-typed`** — no finding. The `type-` reviewer owns this, and
  the crate is already enum-heavy where it counts (`StopReason`, `Status`,
  `ItemKind`, `HaltSource`, `SubmitError`, `SurfaceError`, `ActuatorBackend`,
  `Trigger`, `Anchor`, `LinkStrip`, `Recovery`, `RefusalReason`, `Tab`,
  `TimingPreset`, `Section`, `Stage`, `Proof`). The remaining `String`s are open
  wire vocabularies from the game (`ShopItem::set`, `ShopItem::name`,
  `SubstatReq::name`) where a newtype adds no validation the server can supply.
  See systemic pattern 5 for the one place I would push back, which is a design
  trade rather than a defect.
- **`anti-lock-across-await`** — clean. No guard crosses an `.await` anywhere.
  `session_loop`'s doc states the invariant and every handler `drop(ctrl)`s
  before printing; `heartbeat` says so explicitly; `PipelineBudget::release`
  drops the guard before `notify_waiters()`; `BudgetedChunk::retag_outbound`
  awaits a `Notify`, never a `MutexGuard`. `async-`/`conc-` own the positive.
- **`anti-index-over-iter`** — clean. Not one `for i in 0..len` in the crate.
  The single manual cursor walk (`capture/pcap.rs::ethernet_payload_offset`)
  genuinely needs an index and uses `frame.get(at..at + 2)?`, never `[]`.
- **`anti-collect-intermediate`** — clean. No collect-then-re-iterate chain.
  `InitialBurst::into_ordered` collects twice on purpose (slot order, then
  per-flow `VecDeque`s) and the comment explains the exact-size/`TrustedLen`
  reasoning for each.
- **`anti-string-for-str`** and **`anti-vec-for-slice`** — clean. No `&String`,
  `&Vec<T>`, `&PathBuf`, `&OsString` or `&Box<T>` parameter exists. Paths are
  `impl AsRef<Path>`, journal lines are `&[String]`, `buy_job` takes `&[u8]`.
  Clippy's `ptr_arg` is on by default and silent, which corroborates it.
- **`anti-clone-excessive`** — no finding; `own-` owns it. All 135 `.clone()`
  sites are either handle clones that are cheap *by design and documented as
  such* (`SessionHandles`, `PipelineBudget`, `WatchGate`, `EventLog`,
  `ShutdownSignal`, `Arc<Wpcap>`) or required by an owned-value boundary
  (`config.filter.clone()` into `Controller::new`,
  `Section::Filter(filter.clone())` into `persist`). The only per-frame clones
  are `view_state`'s — covered from a different angle by anti-008.
- **`anti-type-erasure`** — no finding, deliberately. Beyond the two sites the
  `trait-` reviewer owns, there are three more:
  `CaptureSource { packets: Box<dyn PacketSource>, stop: Box<dyn CaptureStop> }`
  (`capture/mod.rs:25-26`), `CaptureWorker::stop` (`app/mod.rs:446`), and
  `capture_loop_budgeted(source: Box<dyn PacketSource>, …)`
  (`app/mod.rs:867`). They are **justified and should be left alone**: the two
  `cfg` arms of `build_source` must agree on a return type, and the erasure is
  the test seam that four fake sources in `app/mod.rs`'s tests depend on.
  Generics would work (`CaptureSource<P, S>`) and would devirtualize one call
  per captured packet — on a path already dominated by the `ip.to_vec()` copy
  in `pcap::capture_loop`, so the win is unmeasurable and the churn touches
  four signatures. Recorded here so a later pass does not "fix" it.
  (`actuator/mod.rs:457`'s `Box<dyn FnMut() + Send>` is test-only.)
- **No proc macros, no `macro_rules!` beyond one local `sym!` helper in
  `pcap::Wpcap::load`** — so nothing in the `macro-` neighbourhood of these
  rules applies.
