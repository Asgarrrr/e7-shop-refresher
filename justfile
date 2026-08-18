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

# Windows only. The backend alone, then the combination actually shipped
# (windivert-backend + gui + actuator): checking the pieces separately misses a
# cfg interaction that only the product build has. `verify` already covers the
# no-backend arm of `app::build_source`. Compiling and testing needs no
# elevation — only launching a capture session does.
backends:
    cargo clippy --locked --no-default-features --features windivert-backend --all-targets -- -D warnings
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked --no-default-features --features windivert-backend
    cargo test --locked
