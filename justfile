set windows-shell := ["powershell.exe", "-NoLogo", "-NoProfile", "-Command"]

# Portable quality suite: the lanes that run on any OS, no native backend, no
# elevation. `backends` below adds the Windows-only lanes.
verify: fmt-check clippy test

fmt-check:
    cargo fmt --all --check

clippy:
    cargo clippy --locked --no-default-features --all-targets -- -D warnings
    cargo clippy --locked --no-default-features --features gui,actuator --all-targets -- -D warnings

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
