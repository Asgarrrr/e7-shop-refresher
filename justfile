set windows-shell := ["powershell.exe", "-NoLogo", "-NoProfile", "-Command"]

# Portable quality suite: the lanes that run on any OS, no native backend, no
# elevation. `backends` below adds the Windows-only lanes.
verify: fmt-check clippy doc test

fmt-check:
    cargo fmt --all --check

# Four lanes: no features, the shipped portable pair, then `gui` and `actuator`
# on their own. Nobody ships either single-feature lane, so those two are not
# product configurations — they are the cfg shapes that a future
# `cfg(feature = "gui")` block nested inside an `all(windows, feature =
# "actuator")` region would break while every shipped combination stayed green.
# Both compile clean today; they are here to keep it that way.
clippy:
    cargo clippy --locked --no-default-features --all-targets -- -D warnings
    cargo clippy --locked --no-default-features --features gui,actuator --all-targets -- -D warnings
    cargo clippy --locked --no-default-features --features gui --all-targets -- -D warnings
    cargo clippy --locked --no-default-features --features actuator --all-targets -- -D warnings

# rustdoc is a lint lane like the others, and it used to be the only one nothing
# ran: the doc-comments in this crate are its design record and they navigate by
# intra-doc links, several of which had rotted unnoticed. The levels live in
# `Cargo.toml`'s `[lints.rustdoc]`, so no `RUSTDOCFLAGS` is needed here and this
# recipe behaves the same in PowerShell and in sh. `--document-private-items`
# because most of the cross-references are between internal items: without it the
# links that matter are not checked at all.
doc:
    cargo doc --locked --no-deps --document-private-items

test:
    cargo test --locked --no-default-features
    cargo test --locked --no-default-features --features gui,actuator

# The release-configuration test lane. Nothing else compiles the
# `#[cfg(not(debug_assertions))]` arm of `stream::budget`'s accounting tests,
# and that arm is the assertion behind `Cargo.toml`'s `overflow-checks = true`:
# a shipped build must saturate and log rather than panic, because a panic from
# a `Drop` during an unwind aborts with no `crash.log`. Debug and release
# genuinely test different code here, so this is a lane and not a duplicate.
#
# Deliberately NOT wired into `verify` for now. Measured on this machine with
# a warm cache, back-to-back `just verify` runs with and without this recipe
# in the dependency list landed anywhere from 1.09x to 3.67x the baseline
# wall-clock across repeated trials (12.5-20s without, 22-46s with; the
# recipe alone costs ~6s in isolation, which does not explain the spread) —
# the variance tracks concurrent cargo/rustc activity from other builds on
# this shared machine, not a stable cost of the lane itself. That crosses
# "roughly doubles" often enough to need a clean re-measurement and a human
# call rather than an automated one made here. Run it explicitly with
# `just test-release`; add it to `verify`'s dependency list once someone
# re-measures on an uncontended machine.
test-release:
    cargo test --locked --release --no-default-features --features gui,actuator

# Windows only. The capture backend on its own, then the combination that
# actually ships — `cargo clippy --locked` with no feature flags, i.e.
# `pcap-backend + gui + actuator`. Checking the pieces separately misses a cfg
# interaction that only a product build has; `verify` already covers the
# no-backend arm. There is no `--all-features` lane any more: with WinDivert
# gone, `--all-features` *is* the default set, and the lanes that existed to
# exercise two coexisting backends had nothing left to arbitrate. Compiling and
# testing needs no elevation — only launching a capture session does.
backends:
    cargo clippy --locked --no-default-features --features pcap-backend --all-targets -- -D warnings
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked --no-default-features --features pcap-backend
    cargo test --locked

# --- measurement: not gates, and deliberately not in `verify` -----------------
#
# `verify` and `backends` answer "is it broken?" and every lane in them is a
# pass/fail a commit must satisfy. The two recipes below answer a different
# question — "would the suite *notice* if it broke?" — and their output is a
# report a human reads, not a verdict. They are kept out of `verify` for three
# separate reasons, each of which alone would be enough:
#
# 1. They need tools that are not part of the toolchain (`cargo-llvm-cov`,
#    `cargo-mutants`), so a `verify` that included them would fail on a clean
#    checkout with an error about a missing subcommand rather than about the
#    code. Every dependency this crate does *not* have was declined with a
#    written argument at its point of temptation — no `proptest`
#    (`capture/ip.rs` and `actuator/plan/geometry.rs`), no `tempfile`, no
#    `arrayvec`/`smallvec` (`stream/reassembly.rs`); these two stay acceptable
#    precisely because they are external binaries and appear in neither
#    `Cargo.toml` nor `Cargo.lock`.
# 2. They are slow in a way the other lanes are not. `coverage` recompiles the
#    crate instrumented; `mutants` recompiles it *once per mutant* — measured at
#    93 mutants for the two `plan` files and 123 for `domain/control/`, on top of
#    a 317 s baseline build.
# 3. Neither produces a number that should ever be a threshold. A coverage
#    percentage turned into a gate is optimised by writing tests that execute
#    lines without asserting anything, which is the exact defect this crate
#    already shipped once — the click-grid guard test below ran every line it
#    cared about and could not fail. The percentage is a map of where to look,
#    not a score.
#
# `.github/workflows/quality.yml` runs both weekly and on demand, for the same
# reasons, and says so in its own header.

# The dependency-policy gate, runnable before CI says no.
#
# Out of `verify` for the first of the three reasons above and only that one:
# `cargo-deny` is an external binary in neither `Cargo.toml` nor `Cargo.lock`,
# so a clean checkout would fail with "no such subcommand" rather than with
# anything about this crate. Unlike `coverage` and `mutants`, what this prints
# *is* a verdict — `deny.toml` is policy with teeth, and its empty `ignore` list
# is meant to stay empty (`deny.toml`'s own header says why).
#
# Version-pinned to match CI exactly, because a different cargo-deny can
# disagree about the same `deny.toml`:
# `cargo install --locked cargo-deny@0.20.2`
deny:
    cargo deny check advisories
    cargo deny check bans licenses sources

# Instrumented line/region coverage on the shipped feature set
# (`pcap-backend + gui + actuator`), because that is the binary a player runs.
#
# Requires `cargo install --locked cargo-llvm-cov` plus
# `rustup component add llvm-tools-preview` — measured at v0.9.0 against this
# crate on Windows, where it works with no crate change at all.
#
# Read the output knowing two things about what it counts, both measured rather
# than assumed:
#
# - The inline `#[cfg(test)] mod tests { … }` block at the bottom of most files
#   *is* counted, and it is always near 100% (test code executes itself), so a
#   per-file percentage here flatters any file whose tests live beside it.
#   Stripping those blocks moves the crate from 84.88% of lines to 72.80% of
#   *production* lines — the per-file, production-only ranking behind that
#   split lives only in the `--html` report below, not in this summary table.
# - A file that is nothing but `#[derive(Deserialize)]` shapes reports no
#   production lines at all — `uplink/protocol.rs` is the whole file — because
#   derive-generated code is attributed to the macro, not to the call site. Zero
#   coverage there means "nothing to measure", not "untested".
#
# `--summary-only` because the per-file table is the useful artefact; add
# `--html --open` locally when chasing one file.
coverage:
    cargo llvm-cov --locked --summary-only

# Mutation testing on the modules where a surviving mutant costs a player money.
#
# This lane exists because of a defect that actually shipped. The guard test
# written for the click-grid invariant — "six clickable rows, the top group is
# 0..=3", whose failure clicks the wrong item's Buy button — had six assertions
# and every one of them was derived from the two constants it was meant to pin.
# `LAST_TOP_ROW = 2`, `= 4` and `MAX_ROW = 7` each passed the entire suite. A
# human caught that by reading; a mutation run catches that whole class by
# construction, because a test that cannot fail cannot kill a mutant either.
#
# Requires `cargo install --locked cargo-mutants` (measured at v27.1.0 on
# Windows against this crate: it needs no configuration file and no source
# change, and it copies the tree to a scratch directory rather than editing the
# working copy).
#
# The scope is five files, not the crate, and the exclusions are as deliberate as
# the inclusions. In: `plan/geometry.rs` and `plan/jobs.rs` (the row/slot/scroll
# arithmetic — a wrong row is a click on the wrong item), and all of
# `domain/control/` (the buy decision, the stop reasons, the recovery ladder — a
# survivor there is a limit that does not stop, a refresh that double-bills, or a
# watchdog that halts blaming the game). Out: the Win32 backends and
# `capture/pcap/`, whose behaviour needs a real window or a live NIC, so a
# survivor there says something about the fixtures rather than about the code;
# and the `ui` modules, where a mutant usually survives because no pixel changed,
# which is not a finding worth a reader's time.
#
# Default features on purpose, and this one is measured rather than assumed:
# `--no-default-features` looks cheaper and is not free, because `egui_kittest`
# is an ungated dev-dependency and every `cargo test` lane therefore still
# builds the egui core anyway (the heavier wgpu/naga/ash stack behind it is now
# gated behind the `render-png` feature and skipped here — see `Cargo.toml`'s
# `[dev-dependencies]` note). What the default lane buys for that same cost is
# a stronger verdict — running the reduced lane reported
# `Controller::is_recovery_enabled -> false` as a survivor, and it is not one:
# the test that kills it (`app::tests::setup_enables_recovery_only_when_live`)
# has two `#[cfg]` bodies and only the `all(windows, feature = "actuator")` one
# asserts `true`.
#
# `--timeout 120` is sixty times the ~2 s the suite takes once built. It is there
# for the mutants that turn a bounded loop unbounded — the one way a mutant hangs
# instead of failing — and not as a performance expectation.
#
# `cargo mutants` exits non-zero when a mutant survives. That is information, not
# a failure: read `mutants.out/missed.txt` and decide per mutant whether the
# expression was unobservable (then it is noise) or whether the test that should
# have noticed cannot fail (then it is the thing this lane was built to find).
#
# No `--shard` here, deliberately: a local run should be complete. The cost is
# that this recipe says nothing about how CI splits the same set — four shards,
# `0/4` through `3/4`, whose matrix lives in `.github/workflows/quality.yml`,
# where an off-by-one once left shard 0 unevaluated while the lane reported
# green. The file list and `--timeout` below are the two halves that *must* stay
# in step with that job; the sharding is the one that must not.
mutants:
    cargo mutants --file src/actuator/plan/geometry.rs --file src/actuator/plan/jobs.rs --file src/domain/control/mod.rs --file src/domain/control/watchdog.rs --file src/domain/control/dedup.rs --timeout 120
