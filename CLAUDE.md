# arkyve-refresh-shop

Local relay that captures Epic Seven Secret Shop traffic (WinDivert), forwards
it to an analysis server, and — rollout in progress — drives a paid refresh
loop from the decoded snapshots. Single-user tool, Windows target.

## Commands

- build: `cargo build` (Windows/WinDivert SDK required)
- test: `cargo test` — single test: `cargo test <name>` — no MSVC/WinDivert: add `--no-default-features`
- lint: `cargo fmt --check && cargo clippy --all-targets -- -D warnings` (repeat clippy with `--no-default-features`)
- run: `cargo run` (Windows, admin rights)

## Architecture map

| Path | Purpose |
|---|---|
| `src/capture/` | WinDivert packet capture + IP/TCP parsing (feature `windivert-backend`) |
| `src/stream.rs` | TCP reassembly (drops retransmissions, resync gate) |
| `src/uplink/` | WebSocket uplink to the analysis server; `protocol.rs` = `ServerMessage` wire shape |
| `src/domain/shop.rs` | Shop model: `ShopSnapshot`, `ShopItem`, `RefreshMeta` |
| `src/domain/filter.rs` | `Filter`: client-side interest criteria (the authoritative verdict) |
| `src/domain/control.rs` | `Controller`: pure refresh-loop state machine (Idle/Watching/Paused/Stopped) |
| `src/watch.rs` + `src/app.rs` | `WatchGate` toggle + pipeline wiring + console rendering |

Data flow: capture → reassembly → uplink (server decodes) → `ServerMessage::Shop(ShopSnapshot)` → render (today) / `Controller` (tranche 3+).

## Don't recreate — it already exists

| Need | Use | Where |
|---|---|---|
| Item interest verdict | `Filter::matches` — never `ShopItem.interesting` (legacy, removal planned) | `src/domain/filter.rs` |
| Refresh-loop decisions | `Controller::handle(Event) -> Vec<Action>` | `src/domain/control.rs` |
| Stop limits / counters / status | `Limits`, `Progress`, `StopReason`, `Status` | `src/domain/control.rs` |
| Refresh balance/cost | `ShopSnapshot.refresh: Option<RefreshMeta>` | `src/domain/shop.rs` |
| Test item fixture | `ShopItem::default()` + struct update syntax | see tests in `src/domain/control.rs` |

Before creating any file, helper, or type: Grep for it, then check this table.
When you build something reusable, add it to this table in the same change.

## Conventions

- `src/domain/` stays pure: no I/O, no clock reads — time arrives as `now_ms` in events.
- Wire models are tolerance-first: `serde(default)` everywhere; partial side-channel objects (e.g. `refresh`) degrade to `None`; one bad field must never kill a message.
- Counters fed by wire data use saturating/checked arithmetic.
- Code, comments, and docs in English; player-facing console text in French.
- Tests in `#[cfg(test)]` modules next to the code; behavioral snake_case names; one behavior per test.

## Controller rollout (tranche plan)

1. ✅ Domain: shop model + `Filter`.
2. ✅ `Controller` state machine — pure, not wired.
3. Wire into `app.rs` (replace `WatchGate`), remove `ShopItem.interesting`, feed real `now_ms` + `Tick`s. Known gaps to solve here: no snapshot identity → duplicate/unsolicited snapshots (hourly auto-refresh) each trigger a paid refresh; Start-before-shop-open ordering contract (see `Event::Start` doc).
4. GUI reading `status()`/`progress()`/`last_snapshot()`/`limits_enforceable()`.
5. Actuator executing `Action::Refresh` — must also update the "fully passive" claim in `lib.rs` docs (and README).

## Verification

Definition of done: `cargo fmt --check` clean, both clippy variants clean with `-D warnings`, `cargo test` green, and the changed behavior pinned by a test written with the change.
