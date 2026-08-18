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
