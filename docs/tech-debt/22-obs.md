# 22 — Observability (`obs-`)

**Category priority:** MEDIUM
**Rules audited:** 7 · **Files read:** 38 of 41 `.rs` files · **Findings:** 15 (P0 1 / P1 5 / P2 5 / P3 4)

> Read whole-file: every module that emits a diagnostic (`main`, `crash`, `journal`,
> `render`, `migrate`, `error`, `lib`, `stream`, `watch`, `config`, `config::persist`,
> `capture/*`, `uplink/*`, `app/*` (non-test), `actuator/{mod,shield,win}`,
> `domain/*` (non-test), `ui/{mod,view,statusbar,journal,shop,theme}`, `build.rs`,
> `examples/ui_preview.rs`) plus `Cargo.toml` and `README.md`'s troubleshooting
> section. Read partially: `actuator/plan.rs` (420/1024), `domain/filter.rs`
> (200/514), `ui/editor/mod.rs` (620/1140). Searched but not read line-by-line:
> `src/domain/control/tests.rs`, `src/app/session/tests.rs`,
> `src/ui/editor/timing_meter.rs` — two grep passes over the whole tree
> (`\b(trace|debug|info|warn|error)!\s*\(` and `println!|eprintln!|dbg!|print!|eprint!`)
> confirm they contain no log or print site, and no `obs-` rule governs test code.

## Verdict

The subscriber setup is *better* than the category average and the two classic P0s
are both absent: the `tracing-appender` `WorkerGuard` really is held for the whole
process (`let _log_guard` is a named binding dropped at the end of `main`, `panic =
"abort"` is deliberately absent so an unwinding `main` still drops it, and there is
no `std::process::exit` anywhere in `src/`), and `crash.rs` really does record and
flush (unbuffered `OpenOptions::append` + `write_all`, two candidate paths, parents
created, hook installed before anything can panic). What is wrong is *what reaches
the file*. **`src/uplink/websocket.rs` is the worst offender**: it logs the raw
`config.server_url` at `info` on every connect and at `warn` on every failed
attempt, while `app::redacted_server_url` exists three files away and `README.md`
line 235 promises the log "never contains the server URL's credentials". That is
the single highest-value fix — one argument, one existing helper. The structural
problem behind the rest is that severity is not expressed at all: the actuator, the
domain and the session loop emit *zero* `tracing` events of their own, so an
actuator halt, a session abort and ">> watching — open the shop" are all one
`info!(target: "journal")` line with the target stripped from the output — the
`RUST_LOG=warn` the README itself suggests silences every one of them.

## Findings

### obs-001 — the un-redacted `server_url` (userinfo + query) is written to the log the player is asked to send us

- **Severity:** P0
- **Rule:** [`obs-no-sensitive-data`](../../.claude/skills/rust-skills/rules/obs-no-sensitive-data.md)
- **Site:** `src/uplink/websocket.rs:112` and `src/uplink/websocket.rs:129` (the raw
  string comes from `src/app/mod.rs:351-357`, which passes `config.server_url.clone()`
  into `uplink::run`)
- **What:**
  ```rust
  info!(url = %url, "server link established");                    // :112, every connect
  warn!(url = %url, error = %err, "server connection failed");     // :129, every failed attempt
  ```
  `url` is `Config::server_url` verbatim. `Config::validate` (`src/config.rs:344-372`)
  accepts any `wss://` URL without inspecting userinfo or query, and
  `app::redacted_server_url` (`src/app/mod.rs:262-267`) exists precisely because such
  a URL can carry a credential — its own test asserts
  `wss://token:secret@ingest.arkyve.dev:8443/path?key=abc` reduces to
  `wss://ingest.arkyve.dev:8443` (`src/app/mod.rs:2229-2231`). `src/app/mod.rs:412`
  uses the helper correctly and `src/main.rs:199-206` deliberately omits the URL with
  a comment saying why. The uplink does neither.
- **Why it matters here:** the default filter is
  `arkyve_refresh_shop=debug,journal=info,warn` — the `info!` passes on the crate
  directive and the `warn!` passes on the global one, so both lines are in the shipped
  file in the shipped configuration. The failure path fires once per reconnect attempt,
  so a server outage writes the credential-bearing URL dozens of times into the file
  `README.md:234-236` instructs the player to email us, under an explicit promise that
  it is not there: *"The log never contains the server URL's credentials — userinfo and
  query string are stripped before anything is written."* That promise is false today.
  It only bites a URL that actually carries a secret (the default `wss://ingest.arkyve.dev/refresh-shop`
  does not), which is why this is a broken invariant rather than an active incident —
  but the invariant is the product's own, documented, and tested-for elsewhere.
- **Fix:** redact once at the boundary and never carry the raw string into the task.
  Make the helper reachable (`pub(crate) fn redacted_server_url`) and have
  `uplink::run` take the display form beside the dial string:
  ```rust
  // app/mod.rs
  workers.spawn("uplink", &fatal_tx, crate::uplink::run(
      config.server_url.clone(),
      redacted_server_url(&config.server_url),   // new: the only form ever logged
      raw_rx, message_tx, config.reconnect_initial(), config.reconnect_max(),
  ));
  // websocket.rs
  info!(server = %display_url, "server link established");
  warn!(server = %display_url, error = %err, "server connection failed");
  ```
  While there, audit the adjacent `UplinkEvent::LinkDown(err.to_string())`
  (`src/uplink/websocket.rs:132`): it becomes a journal line at
  `src/app/session/mod.rs:118-121`, which is mirrored to the same file, so any
  tungstenite variant that ever embeds the URL in its `Display` would leak by the
  same route. Pair the fix with obs-013 so it cannot be reintroduced.
- **Effort:** trivial

### obs-002 — an unwritable log directory silently degrades to an inert sink, and the reason is thrown away

- **Severity:** P1
- **Rule:** [`obs-error-chain`](../../.claude/skills/rust-skills/rules/obs-error-chain.md)
- **Site:** `src/main.rs:80-84`
- **What:**
  ```rust
  // Unwritable log dir: fall back to stdout rather than to no
  // subscriber at all — inert in the windowed build, real in the
  // console one.
  Err(_) => (BoxMakeWriter::new(std::io::stdout), None),
  ```
  The `io::Error` from `rolling::Builder::build` is discarded (`Err(_)`), and nothing
  anywhere reports that file logging is not happening. In the shipped windowed build
  the fallback writer is an inert sink, so the outcome is *total, silent loss of the
  only diagnostic channel*.
- **Why it matters here:** this is a live, measured failure mode, not a hypothetical —
  `src/migrate.rs:1-20` and `build.rs:30-32` both document a machine where
  `%LOCALAPPDATA%\arkyve-refresh-shop` was admins-only and *"`install_logging` could
  not even open its log file … and fell back to an inert stdout"*. The cleanup that
  fixes it is best-effort (`src/migrate.rs:98-111` warns and continues on a failed
  DACL reset), and the directory can also be locked by antivirus, roaming-profile
  policy or a full disk. When it happens the log file is simply absent, and
  `README.md:240-242`'s first triage bullet then gives the *wrong* diagnosis
  ("`arkyve-refresh-shop starting` missing → the app never got to run"): the app ran
  fine. `crash.rs` already solves exactly this problem correctly — `crash_log_paths_from`
  (`src/crash.rs:68-82`) tries `%LOCALAPPDATA%` then the temp dir — while
  `install_logging` only reaches the temp dir when `LOCALAPPDATA` is *unset*, never
  when it is set-but-unwritable.
- **Fix:** two changes, both small.
  1. Mirror `crash.rs`'s candidate list: try `log_dir()`, then
     `std::env::temp_dir().join(APP_DIR).join("logs")`, and only then stdout.
  2. Stop discarding the reason. Return it the way `migrate::Leftovers`
     (`src/migrate.rs:38-71`) already returns findings that predate the subscriber —
     carry the `io::Error` out of `install_logging` and, once the subscriber exists,
     `tracing::error!(error = ?err, dir = %log_dir().display(), "no log file — falling back")`
     plus a journal line so the banner shows it. A player who is told "send the log"
     must be able to learn there is no log.
- **Effort:** small

### obs-003 — the product's two worst events are recorded at `info` on one target, and the target is stripped from the output

- **Severity:** P1
- **Rule:** [`obs-levels-filter`](../../.claude/skills/rust-skills/rules/obs-levels-filter.md)
- **Site:** `src/journal.rs:57-66` (the single-level mirror), plus its severity-bearing
  callers: `src/actuator/mod.rs:400-403` (`fail` — the actuator halted the watch),
  `src/actuator/mod.rs:386-388` (`abort` — a job was abandoned),
  `src/app/session/mod.rs:139` and `:171` (`>> session aborted — {error}`),
  `src/app/session/mod.rs:119-121` (link down). Aggravated by `src/main.rs:97`
  (`.with_target(false)`).
- **What:** `EventLog::emit` is the *only* observability the `actuator`, `domain` and
  session-loop layers have — grep confirms zero `tracing` macros in `src/actuator/**`,
  `src/domain/**` and `src/app/session/mod.rs` apart from the heartbeat — and it emits
  everything at one level and one target:
  ```rust
  tracing::info!(target: "journal", line, "journal");
  ```
  So `>> actuator: the game window runs at a higher integrity level … — stopping the
  loop` and `>> session aborted — capture: …` carry the same level as
  `>> watching — open the shop`. `.with_target(false)` then removes the one field that
  distinguished a player line from a technical one, so the file cannot be triaged by
  level *or* by source.
- **Why it matters here:** `README.md:269-270` tells the reader to *"raise or narrow
  the verbosity with `RUST_LOG`"*, and `README.md:266-267` tells them that when the
  heartbeat stops they should *"look for `session aborted` just before"* — but any
  narrowing to `warn` deletes exactly that line, along with every actuator halt. This
  is the windowed build's only post-mortem channel; a support engineer cannot grep it
  for "what went wrong", cannot sort by severity, and cannot tell the journal target
  from the capture target. The crate clearly knows how to do this — `src/main.rs:227`
  uses `tracing::error!` for a startup failure — the journal path just never got the
  treatment.
- **Fix:** keep the journal line for the player and add a level-carrying event beside
  it at the three non-routine sites. Either directly:
  ```rust
  fn fail(journal: &EventLog, gate: &WatchGate, reason: &str) {
      tracing::error!(reason = %reason, "actuator halted the watch");   // new
      journal.emit(&[format!(">> actuator: {reason} — stopping the loop")]);
      gate.request_halt(HaltSource::ActuatorFailed);
  }
  ```
  (`warn!` in `abort`, `error!` at both `session aborted` sites), or give `EventLog` an
  `emit_at(level, lines)` and route the severity-bearing callers through it. Separately,
  reconsider `.with_target(false)`: a `journal` / `arkyve_refresh_shop` column costs a
  few characters and is what makes the interleaved file readable at all.
- **Effort:** small

### obs-004 — a window that fails to open reports itself only to an inert stderr

- **Severity:** P1
- **Rule:** [`obs-tracing-over-log`](../../.claude/skills/rust-skills/rules/obs-tracing-over-log.md)
- **Site:** `src/main.rs:335-338`, and the same pattern at `src/main.rs:229-232`
- **What:**
  ```rust
  Err(err) => {
      eprintln!("GUI error: {err}");   // :336 — stderr is an inert sink here
      ExitCode::FAILURE
  }
  ```
  `#![cfg_attr(all(windows, feature = "gui"), windows_subsystem = "windows")]`
  (`src/main.rs:6`) makes stderr inert in exactly the build this branch belongs to, and
  no `tracing` event is emitted. Same for `eprintln!("error window failed: {err}")` at
  `:231`, the fallback path taken when even the error window cannot be shown.
- **Why it matters here:** `eframe::run_native` fails for real, common reasons — no GL
  context, a stale display driver, an RDP session, a headless service account. When it
  does, a double-clicked exe shows nothing, exits 1, and the log file the player sends
  contains `arkyve-refresh-shop starting`, `actuator configured` and then *nothing*.
  There is no entry for it in `README.md`'s triage list because no line exists to look
  for. Note `fatal()` (`src/main.rs:224-234`) does the right thing for pre-window
  failures — this branch is the one that was missed.
- **Fix:** log before printing, in both places:
  ```rust
  Err(err) => {
      tracing::error!(error = %err, "the application window could not be created");
      eprintln!("GUI error: {err}");   // still useful in the console lane
      ExitCode::FAILURE
  }
  ```
  and add the resulting line to the `README.md` triage table.
- **Effort:** trivial

### obs-005 — a panic leaves no trace whatsoever in the rotated log

- **Severity:** P1
- **Rule:** [`obs-tracing-over-log`](../../.claude/skills/rust-skills/rules/obs-tracing-over-log.md)
- **Site:** `src/crash.rs:13-35`
- **What:** the panic hook writes `crash.log` and chains the previous hook, but emits
  no `tracing` event. The two channels are completely disjoint: nothing in
  `logs\arkyve-refresh-shop.<date>.log` says a panic happened, when, on which thread,
  or that `crash.log` now exists and should be sent too. A worker-task panic is
  reported to the session loop as `>> session aborted — uplink task panicked`
  (`src/app/mod.rs:497`, journaled at `info`, see obs-003); a panic on the OS main
  thread inside eframe, or in a `Drop` during teardown, produces no log line at all.
- **Why it matters here:** `Cargo.toml:114-117` calls `crash.rs` *"the product's only
  post-mortem channel"* and keeps symbol names in release for it, so the record itself
  is good — but it is only ever read by someone who already knows to look. The rotated
  log is the artefact `README.md:234` asks for first, and it ends abruptly with no
  marker. One line in the hook makes the two files cross-reference each other.
- **Fix:** in the hook, after `write_first_writable`, add
  ```rust
  tracing::error!(
      thread = %thread, location = %location,
      panic = %panic_message(info.payload()),
      "panic — full backtrace in crash.log"
  );
  ```
  It is safe on the hook's own terms: the non-blocking writer is a channel send, and
  a subscriber that is not yet installed makes the macro a no-op (the hook is installed
  before `install_logging` on purpose, `src/main.rs:109-125`), so the file write stays
  the primary record and this is strictly additive.
  Two smaller notes while in the file: `crash.log` is append-only with no cap or
  rotation, unlike `logs\` which keeps 5 files (`src/main.rs:73`) — a crash loop grows
  it without bound; and `crash_entry` stamps raw epoch seconds
  (`src/crash.rs:46-64`) while the log file carries formatted timestamps, so correlating
  the two requires a conversion. Both are P3-grade, batch them with the above.
- **Effort:** trivial

### obs-006 — two GUI error paths use `journal.push`, so their errors die with the process

- **Severity:** P1
- **Rule:** [`obs-error-chain`](../../.claude/skills/rust-skills/rules/obs-error-chain.md)
- **Site:** `src/ui/mod.rs:230-236` and `src/ui/mod.rs:256-261`
- **What:** `EventLog` has two writers: `emit` (ring + `tracing` + console) and `push`
  (ring only — `src/journal.rs:57-92`). Both GUI error paths use `push`:
  ```rust
  if !sections.is_empty()
      && let Err(err) = config::persist::save(&self.config_path, &sections)
  {
      self.handles.journal.push(&[format!("config.toml not saved: {err}")]);
  }
  ```
  and, for a command the bounded queue refused,
  `journal.push(&[">> command dropped — the session is busy, try again"])`. Neither
  reaches the subscriber, so neither survives the window closing.
- **Why it matters here:** the first one is the failure `Error::ConfigWrite` was
  specifically designed to make diagnosable — `src/error.rs:33-42` says *"a read-only
  or antivirus-locked `config.toml` silently discards every Setup change, and the
  banner has to be able to name the file"*. It names it in a 500-entry in-memory ring
  that is gone the moment the player closes the window to come ask why their settings
  keep reverting. The log file, which is what they are asked to send, has no record.
  The `push` here looks deliberate (avoiding the `println!` in `emit`'s console arm),
  which is exactly the kind of accidental downgrade a level-aware `emit_at` (obs-003)
  would remove.
- **Fix:** `tracing::warn!(error = ?err, path = %self.config_path.display(), "config.toml not saved")`
  beside the journal line (and `debug!` for the dropped command), or switch both to the
  level-carrying `emit_at` from obs-003.
- **Effort:** trivial

### obs-007 — the console-only build writes nothing to the console

- **Severity:** P2
- **Rule:** [`obs-levels-filter`](../../.claude/skills/rust-skills/rules/obs-levels-filter.md)
- **Site:** `src/main.rs:67-99`
- **What:** `install_logging` installs a file writer unconditionally. The only stdout
  path is the failure arm at `:83`. So in the `--no-default-features` lane — which has
  a real console, since `windows_subsystem = "windows"` is gated on `feature = "gui"`
  — every `tracing` event still goes to a file under `%LOCALAPPDATA%` (or the system
  temp dir on a mac dev machine), and the terminal shows only the journal's own
  `println!` mirror (`src/journal.rs:63-64`) and the shop dump. `RUST_LOG` changes the
  verbosity but never the destination, so `RUST_LOG=arkyve_refresh_shop=trace` in a
  terminal produces zero terminal output.
- **Why it matters here:** that lane exists for development and CI
  (`README.md:216-218`), and it is the lane where someone is actually watching a
  console. A developer chasing a capture problem has to find
  `/var/folders/…/T/arkyve-refresh-shop/logs/` to read what they just asked for.
- **Fix:** tee, or make the destination selectable. Cheapest correct version:
  ```rust
  #[cfg(not(feature = "gui"))]
  let writer = BoxMakeWriter::new(Tee::new(writer, std::io::stdout));
  ```
  (or honour an `ARKYVE_LOG=stdout` escape hatch, which also helps the windowed build
  when run from a terminal for debugging).
- **Effort:** small

### obs-008 — `fatal()` flattens the error, the path and a multi-line player message into one field

- **Severity:** P2
- **Rule:** [`obs-structured-fields`](../../.claude/skills/rust-skills/rules/obs-structured-fields.md)
- **Site:** `src/main.rs:141-148` and `src/main.rs:224-227`
- **What:**
  ```rust
  return fatal(format!(
      "Invalid configuration: {err}\n\nFix {} and restart.",
      config_path.display()
  ));
  ...
  tracing::error!(reason = %message, "startup failed");
  ```
  The error, the config path and the player-facing instruction are fused into one
  string, and that string contains two embedded newlines — so the default `fmt` layer
  writes a *three-line* record into the file, breaking one-event-per-line for whoever
  greps it. `err` never appears as a field, so the actual cause is unqueryable, and
  `Error::ConfigParse`'s span information is diluted into prose.
- **Why it matters here:** the comment at `src/main.rs:221-223` says these two failures
  (invalid config, runtime that will not build) are *"the likeliest of all"*, which
  makes this the most-read error line in the product. The banner needs the friendly
  multi-line wording; the log needs fields.
- **Fix:** split the two audiences. Log structured, then hand the prose to the window:
  ```rust
  Err(err) => {
      tracing::error!(
          error = ?err,
          config_path = %config_path.display(),
          "invalid configuration"
      );
      return fatal(format!("Invalid configuration: {err}\n\nFix {} and restart.",
                           config_path.display()));
  }
  ```
  and drop the `reason = %message` line from `fatal` (or keep it at `debug`).
- **Effort:** trivial

### obs-009 — errors are logged with `%` (Display), dropping the `source()` chain and the OS error code

- **Severity:** P2
- **Rule:** [`obs-error-chain`](../../.claude/skills/rust-skills/rules/obs-error-chain.md)
- **Site:** `src/app/mod.rs:455`, `:566`, `:883`; `src/uplink/websocket.rs:129`, `:206`,
  `:220`; `src/main.rs:186`; `src/main.rs:227`
- **What:** every error site uses `error = %err`. `crate::Error` carries `#[source]` on
  `ConfigRead`/`ConfigWrite` (`src/error.rs:25-42`) and `#[from]` on `ConfigParse`/`Io`,
  so `Display` does surface *one* level — but `std::io::Error`'s `Display` is only the
  message text, while its `Debug` keeps the structure that identifies the failure:
  `Os { code: 5, kind: PermissionDenied, message: "Access is denied." }` versus
  `Access is denied. (os error 5)`. Deeper chains (a `ConfigWrite` whose source is an
  `Io` wrapping a rename failure) collapse.
- **Why it matters here:** the two failures this crate documents most heavily are an
  unwritable `%LOCALAPPDATA%` (obs-002) and a locked/read-only `config.toml`
  (`src/error.rs:33-42`), and for both the OS error *kind* is the diagnosis — it is what
  separates "antivirus has it open" from "the DACL is wrong" from "the disk is full".
  This is a small, mechanical upgrade, not a rewrite.
- **Fix:** switch the handling boundaries to `error = ?err` (Debug walks the chain per
  the rule's own guidance). Keep `%` only where the value is a plain `String` reason
  (`src/capture/pcap.rs:988`, `src/main.rs:186`'s `keys` field). Do **not** add a second
  log at the propagating layers — the crate is already clean there (see Clean areas).
- **Effort:** trivial

### obs-010 — the first-segment line records the player's client endpoint

- **Severity:** P2
- **Rule:** [`obs-no-sensitive-data`](../../.claude/skills/rust-skills/rules/obs-no-sensitive-data.md)
- **Site:** `src/capture/pcap.rs:660-666`
- **What:**
  ```rust
  info!(
      payload = segment.payload.len(),
      syn = segment.syn,
      server = %segment.flow.server,
      client = %segment.flow.client,          // <- the local endpoint, address included
      "first server-to-client segment admitted"
  );
  ```
  `payload` is a length, which is correct and deliberate. `client` is a full
  `SocketAddr`: on IPv4 that is a private LAN address (harmless), but `capture/ip.rs:30-36`
  parses IPv6 too, where the client address is the machine's globally routable address —
  a stable network identifier for the player, in a file they are asked to email.
- **Why it matters here:** one line per session at `info`, so the exposure is small and
  the line is genuinely load-bearing (`src/capture/pcap.rs:655-659` explains that its
  *absence* is the headline diagnostic). But it does not need the address to do its job:
  the port alone proves the strip, the filter, the port and the adapter choice all agree.
- **Fix:** `client_port = segment.flow.client.port()` instead of `client = %…`, and if
  the full address is ever wanted for a hard case, put it behind `debug!`. Judgement on
  the rest of this dimension: **there is no payload logging anywhere in the crate and no
  hex-dump behind any flag** — see Clean areas.
- **Effort:** trivial

### obs-011 — no spans anywhere: four concurrent tasks and *n* capture threads interleave into one file with no correlation field

- **Severity:** P2
- **Rule:** [`obs-instrument-spans`](../../.claude/skills/rust-skills/rules/obs-instrument-spans.md)
- **Site:** three specific boundaries, in value order:
  1. `src/app/mod.rs:488-505` — `SessionWorkers::spawn` already receives the
     `name: &'static str` it needs (`"uplink"`, `"reassembly"`, `"actuator"`, `"stdin"`),
     and it already wraps the future. One `.instrument()` there tags every event from all
     four tasks:
     ```rust
     let handle = tokio::spawn(async move {
         if AssertUnwindSafe(future).catch_unwind().await.is_err() { … }
     }.instrument(tracing::info_span!("worker", name)));
     ```
  2. `src/capture/pcap.rs:910-1000` — `capture_loop` runs one thread per adapter and
     hand-repeats `device = %short_device_name(&handle.device)` on each of its three
     exit lines, while `poll_drops` (`:1027-1033`) repeats it again. A synchronous
     `let _span = info_span!("adapter", device = %short_device_name(&handle.device)).entered();`
     at the top of the thread body covers all of them (no `.await` in this function, so
     the guard form is correct here — the rule's async pitfall does not apply).
  3. `src/uplink/websocket.rs:109-149` — the reconnect loop has no per-connection
     identity, so `server link established` / `server link interrupted` pairs are
     indistinguishable across a long session. An `attempt` counter as a span field (or
     even just a field on the two events) makes "the 1st reconnect" legible from "the
     40th".
- **What:** the crate contains zero spans. Every line in the file is flat, and the four
  worker tasks plus one thread per adapter all write into it concurrently.
- **Why it matters here:** the log is read post-mortem, once, by someone who was not
  there. `README.md:248-262` asks the reader to correlate `adapter opened and filtered`
  with `the capture driver dropped packets` per adapter and `session heartbeat` with
  `server link down` — all correlations a span field would do for them. The good news:
  because there are no spans, there is also no `span.enter()` held across an `.await`
  anywhere, so the classic async corruption bug is absent.
- **Fix:** the three snippets above. Start with (1) — one line, four tasks.
- **Effort:** small

### obs-012 — the journal mirror carries the whole player line as one opaque field

- **Severity:** P3
- **Rule:** [`obs-structured-fields`](../../.claude/skills/rust-skills/rules/obs-structured-fields.md)
- **Site:** `src/journal.rs:60`
- **What:** `tracing::info!(target: "journal", line, "journal")` — `line` *is* a
  structured field, so this is not a raw violation, but it is a single string containing
  everything: `>> MATCH — slot(s) 1, 2: buy in game — resumes automatically`,
  `>> bought: Covenant Bookmark — 300184000 gold left`, `>> stopped: out of crystals`.
  Nothing in the file is queryable by event kind, so counting matches or buys across a
  session means regexing prose that the render layer is free to reword.
- **Why it matters here:** these are the product's domain events, and the file is the
  only place they persist. It is also the only structured data the crate could ever
  chart (the parked drop-rate gauge would want exactly this).
- **Fix:** optional and cheap — have `emit` take an event kind alongside the text
  (`info!(target: "journal", kind = "match", line, "journal")`), or let the domain emit
  its own typed events beside the prose. Filed at P3 because the current form is
  defensible; do it if obs-003's `emit_at` is implemented, since that touches the same
  signature.
- **Effort:** small

### obs-013 — `Config`'s derived `Debug` will happily print `server_url` again

- **Severity:** P3
- **Rule:** [`obs-no-sensitive-data`](../../.claude/skills/rust-skills/rules/obs-no-sensitive-data.md)
- **Site:** `src/config.rs:41-48`
- **What:** `#[derive(Debug, Clone, Deserialize)] pub struct Config { … pub server_url: String, … }`.
  Nothing logs `?config` today (verified by grep), so this is latent, not live — but the
  type carries no marker at all, and `Config` is exactly the kind of value someone adds
  to a startup line or an `#[instrument]` argument list. obs-001 is what that looks like
  when it happens.
- **Why it matters here:** the rule calls out redacting newtypes precisely because they
  *"protect against accidental `?arg` or `%arg` elsewhere in the codebase"*. The product
  has a documented no-credentials-in-the-log promise (`README.md:235`) enforced today by
  one helper that one of two call sites remembered to use.
- **Fix:** a `ServerUrl` newtype whose `Debug`/`Display` emit only
  `redacted_server_url(...)` (and which exposes the dial string through one named
  method), or at minimum a hand-written `Debug` for `Config` that prints the redacted
  form. This is the structural fix that makes obs-001 unrepeatable.
- **Effort:** small

### obs-014 — session lines printed outside the journal, against `journal.rs`'s own documented rule

- **Severity:** P3
- **Rule:** [`obs-tracing-over-log`](../../.claude/skills/rust-skills/rules/obs-tracing-over-log.md)
- **Site:** `src/render.rs:121-126` (`render_shop`), `src/render.rs:163-165`
  (`print_controls`, called unconditionally at `src/app/mod.rs:415`),
  `src/app/mod.rs:1055-1060` (`println!(">> unknown command: …")`)
- **What:** `src/journal.rs:48-56` states the invariant: *"Single sink for player-facing
  lines … never print session lines around it."* Three sites do:
  - `render_shop` dumps the full item table to stdout on every shop message
    (`src/app/session/mod.rs:321`). Deliberate and documented as console-only, and the
    contents are game data, not secrets — **judge this one acceptable**, but note that it
    means the log file never contains shop contents, which is a real triage limitation
    when a filter "should have matched".
  - `print_controls` is not gated on `not(feature = "gui")`, so the shipped windowed
    build calls it and writes into an inert sink. Dead output; a `#[cfg]` fixes it.
  - the unknown-command line is genuine player feedback that reaches neither the journal
    nor the file. In the console lane the player sees it and we never do; in the windowed
    lane nobody does.

  Not violations, for the record: `build.rs:41,68,69` (cargo directives),
  `src/journal.rs:63-64` (the gated console mirror, which is the correct design),
  `src/main.rs`'s `eprintln!`s as *supplements* to a log line (obs-004 is about them
  being the *only* channel), `src/capture/pcap.rs:1201` and
  `src/ui/editor/mod.rs:1124` (both inside `#[cfg(test)] mod tests`, both `#[ignore]`d).
- **Fix:** gate `print_controls` behind `not(feature = "gui")`; route the unknown-command
  line through `journal.emit`. Leave `render_shop` alone.
  Cross-category note (owned by the `lint-` reviewer, not filed here): `Cargo.toml` has
  no `[lints]` table, so `clippy::print_stdout` / `print_stderr` are off — enabling them
  at `warn` would make this rule mechanical instead of a review item.
- **Effort:** trivial

### obs-015 — `trace` is documented as a verbosity level but nothing ever emits at it

- **Severity:** P3
- **Rule:** [`obs-levels-filter`](../../.claude/skills/rust-skills/rules/obs-levels-filter.md)
- **Site:** `README.md:270` versus the whole of `src/` (zero `trace!` call sites)
- **What:** the troubleshooting section offers `RUST_LOG=arkyve_refresh_shop=trace`. It
  produces byte-identical output to `=debug`. The level table in the rule reserves
  `trace` for per-iteration detail, which is precisely what the two highest-volume loops
  lack: `capture_loop` (`src/app/mod.rs:866`) and `capture_loop`
  (`src/capture/pcap.rs:910`) both summarise every 500–1000 packets
  (`CAPTURE_PROGRESS_EVERY`, `FUNNEL_LOG_EVERY`) with nothing in between.
- **Why it matters here:** small, but it is advice in the support document that does not
  work, and the gap it points at is real — there is no way to see individual segments
  when a capture is misbehaving.
- **Fix:** either add per-segment `trace!` lines with **metadata only** (`seq`,
  `payload_len`, `syn`, `client_port`) — never payload bytes, see obs-010 — or delete the
  `trace` suggestion from `README.md:270`. If lines are added, note that
  `tracing`'s `release_max_level_debug` feature would compile them out of the shipped
  binary entirely, which resolves the "a debug dump still ships in the exe" concern
  before it exists.
- **Effort:** trivial

## Clean areas

- **The `WorkerGuard` is correctly held for the whole process** (`src/main.rs:66-67`,
  `:124-125`). `let _log_guard = install_logging();` is a *named* binding, not `let _ =`,
  so it lives to the end of `main` and drops after `run_mode` returns; `#[must_use]` on
  `install_logging` defends the call site; `Cargo.toml`'s release profile deliberately
  omits `panic = "abort"`, so an unwinding `main` still runs the drop; and there is no
  `std::process::exit` or `abort` anywhere in `src/` that could skip it. This is the
  classic P0 for this category and it is genuinely absent — **do not "fix" it.**
- **`crash.rs` succeeds at recording and flushing.** `append` uses
  `OpenOptions::create(true).append(true)` + `write_all` with no `BufWriter`, so the
  bytes reach the OS before the `File` is dropped at the end of the call; the hook is
  installed before anything can panic (`src/main.rs:109-115`, ahead of the rustls
  provider install and the subscriber); it never panics itself (every failure swallowed,
  two candidate paths, parents created best-effort); it chains the previous hook so the
  console build keeps its stderr trace; and `Cargo.toml:114-117` keeps symbol names in
  release specifically so `Backtrace::force_capture()` is useful. The gap is that the
  *other* file says nothing about it (obs-005), not the recording itself.
- **`EnvFilter` is wired exactly as the rule prescribes** (`src/main.rs:88-93`):
  `try_from_default_env()` with a fallback, not `from_default_env()`, with the comment
  explaining that a malformed `RUST_LOG` must not kill the app — and the fallback
  (`arkyve_refresh_shop=debug,journal=info,warn`) is correct for this binary, including
  the `journal=info` directive the crate-level one would not cover.
- **No payload, key material or TLS byte ever reaches a log.** The audit the brief asked
  for, concretely: there is no `trace!` in the crate; no hex-dump helper exists; the
  only payload-shaped fields logged are *lengths* (`payload = segment.payload.len()` at
  `src/capture/pcap.rs:661`, `bytes` at `src/app/mod.rs:846`, the byte counters in
  `src/stream.rs:590-599`); `Segment`/`BudgetedSegment` (which own the bytes) are never
  passed to a log macro with `?` or `%`; `BudgetedChunk`'s only `Debug` impl is
  `#[cfg(test)]` (`src/stream.rs:364-369`); the four `?`-sigil fields in the whole crate
  are `?stage`, `?strip`, `?since_last_shop_s` and `?snapshot`-free. There are no auth
  headers or session tokens in the client at all — the relay forwards opaque bytes and
  the server interprets them — so there is nothing of that class to leak beyond the URL
  (obs-001).
- **`obs-error-chain`'s log-and-return anti-pattern is absent.** No error is logged at a
  propagating layer and again at the handler. `Config::load` and `persist::save` return
  typed errors without logging; `PcapSource::open` and `open_device` return reasons the
  caller logs once (`src/capture/pcap.rs:556-565`); the capture loop's
  `error!` + `fatal.blocking_send` pair (`src/app/mod.rs:881-887`) is two *audiences*
  (technical log, player journal) for one event, which the crate documents deliberately
  in `src/journal.rs:48-56` — that is a design choice, not double-logging, and it should
  be preserved. Nor is any `Err` swallowed without a record on the pipeline paths: every
  drop, every refusal and every lost click is reported somewhere, enforced by
  `#[must_use]` on `ActuatorHandle::submit` (`src/actuator/mod.rs:82`) and
  `deliver_command` (`src/ui/mod.rs:250`). The two exceptions are obs-002 and obs-006.
- **`obs-levels-filter` is honoured on the technical side.** `error!` is reserved for
  invariant violations and teardown failures (`src/stream.rs:196`, `:727`,
  `src/app/mod.rs:455-460`), `warn!` for recoverable degradation (skipped adapters,
  driver drops, byte pressure, link loss), `debug!` for diagnostic detail (funnel,
  adapter enumeration, capture progress), `info!` for lifecycle only. The volume
  discipline is unusually good: every high-frequency line is rate-limited by an explicit
  constant (`CAPTURE_PROGRESS_EVERY`, `FUNNEL_LOG_EVERY`, `STATS_EVERY_PACKETS`) or by a
  state latch (`outage_reported` in `src/uplink/websocket.rs:107`, `PressureResync` in
  `src/app/mod.rs:70-131`, the downed gate that stops `fail` re-firing per
  `src/actuator/mod.rs:391-399`).
- **`obs-library-facade` is satisfied.** Exactly one `tracing_subscriber` call site
  exists in the crate (`src/main.rs:85`), in the binary that owns `main`; no module,
  test or example installs its own; and `src/migrate.rs:38-71` goes out of its way to
  *defer* emission until the binary's subscriber exists rather than logging into the void.
- **The heartbeat is a genuinely good piece of observability design**
  (`src/app/session/mod.rs:20-22`, `188-212`): `status`, `refreshes`, `gate_armed` and
  `since_last_shop_s` as discrete structured fields, every 30 s, with a doc comment
  naming the three silent failures it disambiguates. It is the model the rest of the
  crate's logging should be measured against.
- **Structured fields are the norm, not the exception.** 34 of the 41 `tracing` call
  sites already use named fields with the right sigils; the only string-interpolated
  messages are `src/migrate.rs:60` (`warn!(target: "migrate", "{warning}")`, a
  pre-formatted best-effort cleanup finding, at most a handful per lifetime of an
  install) and the `journal` mirror (obs-012). This category was expected to be the
  highest-volume finding here and it is not — no grouped `info!("thing {}", x)` finding
  was needed.

## Not applicable

- **`obs-library-facade`'s library half** — `publish = false`, one binary, one subscriber
  install. Confirmed clean above rather than skipped.
- **PII/GDPR-class data** — the crate handles no user accounts, emails or personal
  records; the only quasi-identifiers in reach are the local network endpoint (obs-010)
  and the expanded `%APPDATA%`/`%LOCALAPPDATA%` paths, which contain the Windows user
  name. The paths are logged deliberately (`src/main.rs:137`, and `src/error.rs:20-24`
  explains why the file must be named), the player sends the file voluntarily to get
  support, and no lesser form would be actionable — **considered and accepted, not a
  finding.**
- **Metrics/OpenTelemetry exporters, `tracing-error`/`SpanTrace`** — no rule in this
  category requires them, and a single-player desktop relay has no aggregator to export
  to. `PipelineStats` (`src/stream.rs:75-85`) already carries the counters that would
  feed one, if that ever changes.
- **Test-code logging** — `#[cfg(test)]` blocks and the two dedicated `tests.rs` modules
  are outside every `obs-` rule; verified to contain no log site regardless.
