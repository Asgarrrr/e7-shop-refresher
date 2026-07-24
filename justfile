# Full green-check: the platform-independent lanes CI runs.
# The native windivert-backend build needs admin + a kernel driver — run it
# manually: `cargo build --release` (see README).
verify: fmt-check clippy test

fmt-check:
    cargo fmt --all --check

clippy:
    cargo clippy --no-default-features -- -D warnings
    cargo clippy --no-default-features --features gui,actuator -- -D warnings

test:
    cargo test --no-default-features
    cargo test --no-default-features --features gui,actuator
