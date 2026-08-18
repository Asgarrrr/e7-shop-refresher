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

# Windows only. Every capture backend, alone and in the product combination it
# ships (or shipped) in — `cargo clippy --locked` with no feature flags is the
# current default, `pcap-backend + gui + actuator`. Checking the pieces
# separately misses a cfg interaction that only a product build has, and
# `--all-features` is its own lane because compiling *both* backends at once is
# the only thing that exercises the precedence gates: the `not(feature =
# "pcap-backend")` cfgs in `capture::mod` and the arm ordering in
# `app::build_source`. `verify` already covers the no-backend arm. Compiling and
# testing needs no elevation — only launching a capture session does.
backends:
    cargo clippy --locked --no-default-features --features pcap-backend --all-targets -- -D warnings
    cargo clippy --locked --no-default-features --features windivert-backend --all-targets -- -D warnings
    cargo clippy --locked --no-default-features --features windivert-backend,gui,actuator --all-targets -- -D warnings
    cargo clippy --locked --all-targets -- -D warnings
    cargo clippy --locked --all-features --all-targets -- -D warnings
    cargo test --locked --no-default-features --features pcap-backend
    cargo test --locked --no-default-features --features windivert-backend
    cargo test --locked --no-default-features --features windivert-backend,gui,actuator
    cargo test --locked
    cargo test --locked --all-features
