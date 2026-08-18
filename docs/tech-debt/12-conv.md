# 12 — Conversions (`conv-`)

**Category priority:** MEDIUM
**Rules audited:** 3 · **Files read:** 42 · **Findings:** 4 (P0 0 / P1 0 / P2 1 / P3 3)

## Verdict

The crate contains **not a single `impl From`, `TryFrom`, `FromStr`, `AsRef` or `AsMut`**
(`grep -rn "^impl"` over `src/`, `build.rs`, `examples/`): every conversion is a plain
function or a serde derive. For a binary that is mostly defensible — nothing outside the
crate calls these — and the two conversion surfaces that matter most are already right:
wire decoding is all serde derive plus `deserialize_with` (no hand-rolled `from_json`),
and every narrowing numeric cast goes through `TryFrom` with a comment (`src/stream.rs:726`
is exemplary). The worst offender is `src/actuator/plan.rs:130`, `to_screen`, whose
`Result<(i32, i32), String>` error is the one place a stringly conversion failure costs
something concrete: the executor cannot match on it, so it re-implements the
degenerate-rect test upfront (`src/actuator/mod.rs:305`) and classifies everything the
converter returns as `Fatal`. **Highest-value fix: give `to_screen` a two-variant error
enum (conv-001)** — it removes a duplicated invariant check and lets the failure classify
itself. `conv-asmut-mutable` is clean, and provably so, not by accident.

## Findings

### conv-001 — `to_screen` is a fallible conversion with a `String` error, so its two failure modes cannot be told apart

- **Severity:** P2
- **Rule:** [`conv-tryfrom-fallible`](../../.claude/skills/rust-skills/rules/conv-tryfrom-fallible.md)
- **Site:** `src/actuator/plan.rs:130` (definition); consumers `src/actuator/mod.rs:318`, `src/actuator/mod.rs:305`, `src/actuator/win.rs:198`, `src/actuator/win.rs:627`
- **What:** `pub fn to_screen(rect: ClientRect, point: DesignPoint) -> Result<(i32, i32), String>`
  returns two textually distinct failures — `"degenerate client area {w}×{h}"` and
  `"window aspect {a:.3} is narrower than 16:9 — widen the game window"` — through one
  `String`. The rule's Notes are explicit: *"Use a concrete error type, not `String` or
  `Box<dyn Error>`, so callers can match on it."*
- **Why it matters here:** the two failures want **different `SurfaceError` classifications**,
  and because the caller cannot match, it gets them by duplicating the test instead:
  - `src/actuator/mod.rs:305` re-tests `rect.width <= 0 || rect.height <= 0` right after
    `acquire` and calls `abort` (recoverable — a minimized window self-heals on the next
    acquire), purely so that case never reaches `to_screen`;
  - `src/actuator/mod.rs:318` then treats *everything* `to_screen` returns as
    `fail` → `SurfaceError::Fatal` → `WatchGate::request_halt`, i.e. the watch stops.
  - the same degenerate/minimized test is spelled a third and fourth time in
    `src/actuator/win.rs:198` and `src/actuator/win.rs:627`.
  So one invariant lives in four places, and the only thing keeping a minimized window
  from halting the loop instead of aborting one job is that the duplicate check runs first.
  Delete or reorder that pre-check during a refactor and a transient minimize becomes a
  hard halt with no compiler complaint.
- **Fix:** a concrete error, matched at the one site that owns the policy:
  ```rust
  // plan.rs
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum ScreenError {
      DegenerateRect { width: i32, height: i32 },
      TooNarrow,          // carries the aspect if the message wants it
  }
  impl std::fmt::Display for ScreenError { /* the two strings, unchanged */ }
  pub fn to_screen(rect: ClientRect, point: DesignPoint) -> Result<(i32, i32), ScreenError>
  ```
  then in `actuator/mod.rs`, drop the `rect.width <= 0` pre-check and let the match decide:
  `Err(ScreenError::DegenerateRect { .. }) => abort(..)`,
  `Err(ScreenError::TooNarrow) => fail(..)`. Existing `Display` output is preserved, so the
  journal assertions (`"narrower than 16:9"`, `"degenerate client area 0×0"` in
  `src/actuator/mod.rs:1158` and `:1086`) keep passing.
  A `impl TryFrom<(ClientRect, DesignPoint)> for ScreenPoint` on top is optional and
  probably not worth it — this is a two-input transform, and the tuple impl reads worse
  than the named function. **The error type is the part the rule actually buys here.**
- **Effort:** small

### conv-002 — three `Option`-returning converters named `from_*`/`for_*` that `TryFrom` would express

- **Severity:** P3
- **Rule:** [`conv-tryfrom-fallible`](../../.claude/skills/rust-skills/rules/conv-tryfrom-fallible.md)
- **Site:** `src/capture/pcap.rs:343` (`LinkStrip::for_datalink`), `src/actuator/plan.rs:376` (`TimingPreset::from_timings`), `src/domain/control/dedup.rs:23` (`fingerprint`)
- **What:** three single-input fallible converters, all shaped like the rule's Bad example
  (`fn port_from_u32(n: u32) -> Result<Port, String>`) but returning `Option` instead:
  - `fn for_datalink(datalink: c_int) -> Option<Self>` — integer → enum, the classic
    `TryFrom<c_int>` case;
  - `fn from_timings(timings: &Timings) -> Option<TimingPreset>` — `TryFrom<&Timings>`;
  - `fn fingerprint(snapshot: &ShopSnapshot) -> Option<Vec<SlotIdentity>>`.
- **Why it matters here:** all three are `pub(crate)`/private with 1–2 call sites, so the
  cost is idiom and discoverability rather than ergonomics — this is a nit, filed because the
  `from_`/`for_` naming already claims to be a conversion while opting out of the trait that
  defines one. Verdicts differ per site, and two of them are arguably fine as they stand:
  - **`for_datalink` is the one worth converting.** `impl TryFrom<c_int> for LinkStrip` with
    `type Error = UnsupportedDatalink(c_int)` reads better and the sole caller
    (`src/capture/pcap.rs:772`) already builds its message from the raw `datalink` value.
  - **`from_timings`**: `Option` is honest — "no preset matches" is not an error, it is the
    `Custom` mode both callers render (`src/ui/editor/mod.rs:212`, `:469`). `TryFrom` would
    need an inhabited-but-meaningless error type. Leave it, or rename it away from `from_`.
  - **`fingerprint`**: `impl TryFrom<&ShopSnapshot> for Vec<SlotIdentity>` is **forbidden by
    the orphan rule** (`Vec<Local>` is not a local type), so it would first need a
    `Fingerprint(Vec<SlotIdentity>)` newtype. Not worth it for one call site; the name is
    already not a conversion name, which is the right call.
- **Fix:** convert `for_datalink` to `TryFrom<c_int>`; leave the other two, and if anything
  changes there, change the *names* rather than the shapes.
- **Effort:** trivial

### conv-003 — `parse_command` is the crate's only string parser and is not `FromStr`

- **Severity:** P3
- **Rule:** [`conv-fromstr-parsing`](../../.claude/skills/rust-skills/rules/conv-fromstr-parsing.md)
- **Site:** `src/app/mod.rs:1068`, sole caller `src/app/mod.rs:1051`
- **What:** `fn parse_command(line: &str) -> Option<Command>` maps `"start"|"on"` →
  `Command::Start`, `"off"|"stop"` → `Command::Stop`, `""|"t"|"toggle"` →
  `Command::Toggle`, everything else → `None`. This is the rule's Bad example almost
  verbatim (`fn parse_color(s: &str) -> Result<Color, String>`).
- **Why it matters here:** honestly, not much — there is no clap, no serde path into
  `Command`, and one caller. The one concrete consequence is that the unknown-input wording
  lives at the call site (`src/app/mod.rs:1057`, `">> unknown command: {:?} (start, stop, enter = toggle)"`)
  rather than next to the alias table it describes, so the two can drift; a
  `ParseCommandError` with a `Display` would keep them together. `Command` also carries
  three `Set*` variants that no string can produce, which `FromStr` documents naturally as
  "the parseable subset" rather than hiding in a private function.
- **Fix:** `impl FromStr for Command { type Err = ParseCommandError; }` with the same match
  body and `Err(ParseCommandError(other.to_owned()))` in the fallback; the caller becomes
  `match line.parse::<Command>()` and prints the error's `Display`. The three existing tests
  (`src/app/mod.rs:2241`–`2265`) port unchanged apart from `Some(..)`/`Ok(..)`.
- **Effort:** trivial

### conv-004 — `Error::Io` is a context-free `From<std::io::Error>` that wins in every `?` position

- **Severity:** P3
- **Rule:** [`conv-tryfrom-fallible`](../../.claude/skills/rust-skills/rules/conv-tryfrom-fallible.md) (conversion *shape*; the error-type design belongs to the `err-` reviewer)
- **Site:** `src/error.rs:67` (`#[error("i/o: {0}")] Io(#[from] std::io::Error)`), only current beneficiary `src/app/mod.rs:590`
- **What:** three variants of `Error` wrap `std::io::Error`: `ConfigRead { path, source }`,
  `ConfigWrite { path, source }`, and `Io`. Only the last one is `#[from]`, so it is the one
  `?` picks — and it is the one that throws the context away. `src/error.rs:23` documents
  exactly why that matters: *"The path is carried because the file lives out of the way in
  `%APPDATA%`: a bare 'Access is denied. (os error 5)' would leave the player nothing to fix."*
- **Why it matters here:** this is preventive, not a live bug. There is exactly one `?` that
  uses the impl today (`std::thread::Builder::spawn` in `spawn_capture_with_budget`), where
  no path would help anyway, and every filesystem site correctly hand-builds its variant with
  `map_err` (`src/config.rs:337`, `src/config/persist.rs:57`, `:84`, `:95`, `:162`). The
  hazard is that the impl makes the lossy conversion the *default*: the next `?` on an
  `io::Error` in a function returning `crate::Result` silently reintroduces the pathless
  message this crate already fixed once. Note this is also *why* the Win32 side uses named
  functions rather than `From` — `preflight_refusal`, `post_refusal` and `placement_refusal`
  are three different conversions from one `std::io::Error`, and `From` can only express one
  of them (see Clean areas). `Error` did not get the same treatment.
- **Fix:** drop `#[from]` from `Io` (keep the variant), and make the single site explicit:
  ```rust
  .spawn(...)
  .map_err(|source| Error::Capture(format!("spawning the capture thread: {source}")))?
  ```
  After that, every `io::Error` → `Error` conversion in the crate is a deliberate one, and
  the compiler flags a future `?` instead of silently degrading it.
- **Effort:** trivial

## Clean areas

- **`conv-asmut-mutable`: no violations, and not by luck.** Every `&mut Vec<T>` parameter in
  the crate is a *growable sink* that pushes or removes — `lines: &mut Vec<String>`
  (`src/app/session/mod.rs:468`, `:489`, `:513`, `:533`, `:569`, `:585`, `:633`),
  `out: &mut Vec<BudgetedChunk>` (`src/stream.rs:711`, `:777`), `removed: &mut Vec<String>`
  (`src/config/persist.rs:229`), the editor's list widgets (`src/ui/editor/mod.rs:288`,
  `:692`, `:723`). `impl AsMut<[T]>` cannot serve any of them: a slice cannot grow. The rule's
  "When Not to Use" also covers the rest — `&mut Jitter`, `&mut EditorState`, `&mut Timings`
  and `&mut egui::Ui` are domain/foreign types with one shape each, and `&mut RECT` /
  `&mut BpfProgram` / `&mut PcapStat` / `&mut errbuf` are FFI out-parameters that must stay
  concrete. Nothing here should be touched.
- **The one genuine buffer-shaped parameter already takes a slice:**
  `timing_group(ui, title, rows: &mut [(&str, &mut DelayRange, u64)])`
  (`src/ui/editor/timing_meter.rs:65`) — callers pass `&mut [ .. ]` arrays
  (`src/ui/editor/mod.rs:552`, `:565`, `:587`) and unsized coercion handles it, so `AsMut`
  would add generics for zero flexibility.
- **Narrowing conversions use `TryFrom`, not `as`,** with the reasoning written down:
  `usize::try_from(self.next_off - offset)` (`src/stream.rs:726`, with a five-line comment on
  what an `as` cast would silently do), `i16::try_from` in `pack_point`
  (`src/actuator/win.rs:720`), `u8::try_from(index + 1)` in `effective_slot`
  (`src/domain/shop.rs:124`), `u64::try_from` in `record_drop` (`src/stream.rs:229`) and
  `EventLog::now_ms` (`src/journal.rs:45`).
- **Error classification is deliberately *not* `From`, and that is correct:**
  `preflight_refusal` (`src/actuator/win.rs:431`), `post_refusal` (`:545`) and
  `placement_refusal` (`src/actuator/shield.rs:116`) are three different
  `&std::io::Error → SurfaceError`/`String` mappings that turn on the same
  `ERROR_ACCESS_DENIED`. Coherence allows exactly one `From<&io::Error>`, so named functions
  are the only shape that works — and each carries a comment explaining which situation it
  serves. Do not "simplify" these into a `From` impl.
- **`impl AsRef<Path>` on the filesystem entry points** — `Config::load`
  (`src/config.rs:326`), `persist::save` (`src/config/persist.rs:51`),
  `persist::strip_retired_keys` (`:156`) — the read-side counterpart of this category, done
  right; callers pass `&Path`, `&PathBuf` and `PathBuf` interchangeably in the tests.
- **Wire → domain conversion is entirely serde,** no hand-rolled decoders: `ServerMessage`
  (`src/uplink/protocol.rs:12`), `ShopSnapshot`/`ShopItem`/`ItemKind`
  (`src/domain/shop.rs`), with `object_or_none` / `lenient_elements` as
  `deserialize_with` hooks rather than `fn shop_from_json(..)` functions. There is no
  conversion debt on the wire boundary at all.

## Not applicable

- **`conv-fromstr-parsing` for the config and wire enums** (`ActuatorBackend`,
  `ItemKind`): both are `Deserialize` with `rename_all = "snake_case"` /
  `#[serde(other)]`, and TOML/JSON deserialization never routes through `FromStr`. There is
  no clap or argh in `Cargo.toml` and the binary takes no arguments, so an added `FromStr`
  would be dead code with a second, divergent spelling of the same mapping.
- **`parse_segment` (`src/capture/ip.rs:20`) stays a function.** It takes two inputs
  (`bytes: &[u8]`, `game_port: u16`) and returns `Option`, so `TryFrom` would need a tuple
  source type and an invented error; a packet that is "not the game server talking" is a
  filter miss, not a conversion failure.
- **`wide()` (`src/actuator/win.rs:45`, `src/capture/pcap.rs:890`) and the two inline
  `encode_wide` copies (`src/migrate.rs:166`, `:239`)** cannot be trait impls — `Vec<u16>` is
  foreign, so the orphan rule blocks `From<&str> for Vec<u16>`. *Adjacent note for whoever
  owns duplication (not a `conv-` finding):* that is four copies of the same `&str`/`&OsStr`
  → NUL-terminated UTF-16 conversion, and only one of them carries the `#[must_use]` plus the
  "the buffer *is* the value" dangling-pointer warning.
