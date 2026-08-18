# Audit brief — shared instructions for every category reviewer

You are one of 26 reviewers auditing this crate against the `rust-skills` skill.
You own **exactly one category**. Another agent owns each of the others — do not
report findings that belong to a sibling category, and do not widen your scope.

## Your rules

The skill lives at `.claude/skills/rust-skills/`.

1. Read `.claude/skills/rust-skills/SKILL.md` and locate your category's table row.
2. `ls .claude/skills/rust-skills/rules/ | grep '^<your-prefix>'` to get your rule files.
3. **Read every one of your rule files in full** before reviewing any source.
   The rule files carry Bad/Good examples that define exactly what counts as a
   violation. Do not audit from the one-line summary in SKILL.md.

## The source under review

Every `.rs` file in the crate, largest first:

```
2266 src/app/mod.rs              599 src/ui/mod.rs             226 src/capture/ip.rs
1781 src/domain/control/tests.rs 514 src/domain/filter.rs      225 src/ui/shop.rs
1510 src/actuator/win.rs         460 src/uplink/websocket.rs   218 src/watch.rs
1423 src/app/session/tests.rs    367 src/ui/theme.rs           214 src/ui/journal.rs
1418 src/stream.rs               340 src/main.rs               191 examples/ui_preview.rs
1274 src/config.rs               323 src/migrate.rs            170 src/crash.rs
1220 src/actuator/mod.rs         323 src/actuator/shield.rs    158 src/journal.rs
1213 src/capture/pcap.rs         293 src/ui/statusbar.rs       158 src/domain/control/watchdog.rs
1140 src/ui/editor/mod.rs        291 src/ui/view.rs            102 src/capture/mod.rs
1024 src/actuator/plan.rs        251 src/ui/editor/timing_meter.rs  84 src/uplink/protocol.rs
 876 src/config/persist.rs       240 src/render.rs              72 build.rs
 718 src/domain/control/mod.rs   228 src/domain/shop.rs         68 src/error.rs
 673 src/app/session/mod.rs                                     41 src/lib.rs
                                                                37 src/domain/control/dedup.rs
                                                                21 src/uplink/mod.rs
                                                                 7 src/domain/mod.rs
```

Also read `Cargo.toml` (features, profiles, lints) — several categories are
decided there.

**Read every file, one by one, in full.** That is the explicit instruction from
the user who commissioned this audit. Do not sample, do not grep-and-skip. Large
files can be read in two Read calls with `offset`/`limit`; read the whole thing.
You may grep first to *prioritise*, never to *substitute* for reading.

## Project context you must respect

This is a Windows-first desktop app: it sniffs Epic Seven Secret Shop traffic
off the wire (Npcap via runtime-loaded `wpcap.dll`), reassembles TLS record
boundaries, relays to a server over `wss://`, and drives the game window with
synthetic clicks. It is a **binary, not a published library** — `publish = false`.

Consequences you must apply when judging severity:

- API-design and documentation rules aimed at *public library surface* are
  weaker here. `pub` on an internal item is not a public API. Say so instead of
  filing 40 findings about missing `#[non_exhaustive]`.
- FFI (`windows-sys`, `libloading`) makes some `unsafe`, `as` casts and
  `#[repr]` choices load-bearing. Read the surrounding comment before calling
  something a defect — this codebase documents its Win32 traps deliberately.
- The comments in this crate are unusually dense and explanatory *on purpose*.
  Do not file "over-commented" or propose deleting rationale comments.
- Tests live both in `#[cfg(test)] mod tests` and in dedicated `tests.rs`
  submodules (`src/domain/control/tests.rs`, `src/app/session/tests.rs`).

## What counts as a finding

A finding must be a **concrete, located defect or debt**, not a vibe. Each one
needs a file, a line, the rule it violates, why it matters *here*, and a fix.

Do not file:
- Anything you have not read the surrounding code for.
- Style preferences that the rule files do not actually state.
- Speculation about code paths you could not find.
- Duplicate findings across many sites — collapse them into one finding with a
  site list, and count them once.

Be honest about a clean result. If your category genuinely has no violations
(entirely plausible for `macro-`, `const-`, `conc-`, `unsafe-` in parts of this
crate), say so and produce a short report. A padded report is worse than an
empty one — a later agent will act on what you write.

## Severity scale

| Severity | Meaning |
|---|---|
| `P0` | Bug, unsoundness, panic on reachable input, or data loss. Fix now. |
| `P1` | Real defect or debt with user-visible or maintenance cost. Fix soon. |
| `P2` | Idiom/robustness improvement worth doing when touching the file. |
| `P3` | Nit. Batchable cleanup. |

Category priority (CRITICAL/HIGH/MEDIUM/LOW) is *not* severity. A `name-` nit is
P3 even though naming is MEDIUM; a `type-` bug is P0 even though type-safety is
MEDIUM.

## Output

Write **one file**: `docs/tech-debt/<NN>-<prefix>.md` (your prompt gives `NN`
and `prefix`). Overwrite it if it exists. Use exactly this shape:

```markdown
# <NN> — <Category name> (`<prefix>`)

**Category priority:** <CRITICAL|HIGH|MEDIUM|LOW|REFERENCE>
**Rules audited:** <n> · **Files read:** <n> · **Findings:** <n> (P0 <n> / P1 <n> / P2 <n> / P3 <n>)

## Verdict

<2–5 sentences. The honest state of this category in this crate. Name the worst
offender file and the single highest-value fix.>

## Findings

### <prefix>-NNN — <one-line title>

- **Severity:** P<n>
- **Rule:** [`<rule-id>`](../../.claude/skills/rust-skills/rules/<rule-id>.md)
- **Site:** `src/foo.rs:123` (+ other sites, if collapsed)
- **What:** <the defect, concretely — quote the offending line if short>
- **Why it matters here:** <consequence in *this* codebase, not in the abstract>
- **Fix:** <the specific change; a code sketch if it is not obvious>
- **Effort:** <trivial | small | medium | large>

<repeat, ordered most severe first>

## Clean areas

<Rules you audited that this crate already honours, and where it does so well.
One line each, grouped. This section is not optional — it stops the next reader
from "fixing" something that is already right.>

## Not applicable

<Rules that do not apply, with the one-line reason (e.g. "no proc macros in this
crate"). Keeps the audit auditable.>
```

Number your findings `<prefix>-001`, `<prefix>-002`, … so the synthesis index can
reference them stably.

## Rules of engagement

- **Read-only on source.** Do not edit, fix, or refactor any `.rs` file, any
  `Cargo.toml`, or any file outside your one report. You produce a report; a
  later pass implements. Do not run `cargo fix`, `cargo clippy --fix`, or
  `cargo fmt`.
- Running read-only commands is fine and encouraged where it sharpens a finding:
  `cargo clippy`, `cargo tree`, `grep`. Prefer evidence over assertion — if
  clippy already flags something, cite it.
- Never create a git worktree for this task.

Your final message back to the orchestrator should be **≤ 20 lines**: the verdict
sentence, the P0/P1 count, and the titles of your P0/P1 findings only. The full
detail belongs in your report file.
