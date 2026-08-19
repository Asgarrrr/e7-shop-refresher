//! Dashboard preview with injected mock data.
//!
//! On a machine without the capture backend (`pcap-backend` is Windows-only;
//! mac dev), no shop snapshot ever arrives, so the live window only shows the
//! welcome screen. This example builds the real `ShopApp` over a hand-seeded
//! controller — same rendering path as production, fed fixtures — so the
//! status bar, slot table, currencies and journal can be seen and clicked.
//!
//! Run:
//!
//! ```text
//! cargo run --example ui_preview --no-default-features --features gui
//! ```
//!
//! Not part of the shipped binary; nothing here touches production wiring.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use arkyve_refresh_shop::app::{Command, SessionHandles};
use arkyve_refresh_shop::domain::control::{Controller, Event, Limits, Status};
use arkyve_refresh_shop::domain::filter::Filter;
use arkyve_refresh_shop::domain::shop::{
    CatalogId, Crystals, Gold, ItemKind, PurchaseLimit, RefreshMeta, ShopItem, ShopSnapshot,
};
use arkyve_refresh_shop::journal::EventLog;
use arkyve_refresh_shop::ui::{SessionErrorSlot, ShopApp};
use arkyve_refresh_shop::watch::WatchGate;

use eframe::egui;
use tokio::sync::mpsc;

/// A shop the default hunt would care about: the two covenant/mystic tokens
/// the example filter targets (they render as matched — green), plus filler
/// gear, a hero, and one sold-out slot to exercise every row style.
fn mock_snapshot() -> ShopSnapshot {
    let item = |id: u32, slot: u8, kind: ItemKind, name: &str, price: u32| ShopItem {
        id: CatalogId::new(id),
        slot,
        kind,
        name: Some(name.to_owned()),
        price: Some(Gold::new(price)),
        ..Default::default()
    };
    ShopSnapshot {
        merchant: Some("Secret Shop".to_owned()),
        slots: vec![
            item(101, 1, ItemKind::Token, "ticketrare_name", 184_000),
            item(102, 2, ItemKind::Token, "ticketspecial_name", 280_000),
            ShopItem {
                grade: Some(4),
                set: Some("set_speed".to_owned()),
                ..item(201, 3, ItemKind::Equipment, "Wondrous Potion Vial", 51_000)
            },
            ShopItem {
                grade: Some(3),
                ..item(202, 4, ItemKind::Equipment, "Ring of the Fallen", 48_500)
            },
            ShopItem {
                limit: Some(PurchaseLimit {
                    remaining: 0,
                    total: 1,
                }),
                ..item(301, 5, ItemKind::Token, "friendpoint_name", 18_000)
            },
            item(203, 6, ItemKind::Hero, "Mercenary Fighter", 30_000),
        ],
        refresh: Some(RefreshMeta {
            crystal_balance: Crystals::new(20_000),
            cost: Crystals::new(3),
        }),
    }
}

fn main() -> eframe::Result {
    // The example filter targets the covenant bookmark and mystic medal by
    // their internal names, so those two slots read as matched.
    let filter = Filter {
        names: vec![
            "ticketrare_name".to_owned(),
            "ticketspecial_name".to_owned(),
        ],
        ..Filter::default()
    };
    let limits = Limits {
        max_refreshes: Some(10),
        max_spend: Some(Crystals::new(30)),
        max_matches: Some(5),
        ..Limits::default()
    };

    let journal = EventLog::default();
    let controller = Arc::new(Mutex::new(Controller::new(filter, limits)));
    let gate = WatchGate::new(false);

    // Seed a live run: arm, deliver the shop, then echo purchases so the haul
    // reads Covenant 1 / Mystic 0 / +1 other. Token 101 (covenant) is bought;
    // 102 (mystic) stays unbought so a still-green matched row shows.
    {
        let mut ctrl = controller.lock().expect("controller mutex");
        // Each `handle` returns the actions the session would run; the
        // preview has no actuator, so every result is dropped on purpose.
        let _ = ctrl.handle(Event::Start {
            now_ms: journal.now_ms(),
        });
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: mock_snapshot(),
            now_ms: journal.now_ms(),
        });
        let _ = ctrl.handle(Event::Purchase {
            item: CatalogId::new(101),
            gold: Some(Gold::new(300_184_000)),
            now_ms: journal.now_ms(),
        });
        let _ = ctrl.handle(Event::Purchase {
            item: CatalogId::new(201),
            gold: Some(Gold::new(300_000_000)),
            now_ms: journal.now_ms(),
        });
        gate.set(matches!(ctrl.status(), Status::Watching | Status::Paused));
    }
    journal.push(&[
        "armed — watching the Secret Shop".to_owned(),
        "shop captured · 6 slots".to_owned(),
        "match · slot 1 · ticketrare_name · 184,000 gold".to_owned(),
        "match · slot 2 · ticketspecial_name · 280,000 gold".to_owned(),
        "bought · ticketrare_name · 300,184,000 gold left".to_owned(),
        "bought · Wondrous Potion Vial · 300,000,000 gold left".to_owned(),
    ]);

    let (commands, mut receiver) = mpsc::channel::<Command>(16);
    let handles = SessionHandles {
        controller: controller.clone(),
        commands,
        gate: gate.clone(),
        journal: journal.clone(),
    };

    // Make the buttons real: drain the command channel and apply each command
    // to the seeded controller, exactly as the session loop would (minus the
    // actuator and journal feedback). try_recv polling needs no tokio runtime.
    std::thread::spawn(move || {
        let clock = journal;
        loop {
            match receiver.try_recv() {
                Ok(command) => {
                    let mut ctrl = controller.lock().expect("controller mutex");
                    let now_ms = clock.now_ms();
                    let event = match command {
                        Command::Start => Some(Event::Start { now_ms }),
                        Command::Stop => Some(Event::Stop),
                        // Exhaustive, like the production twin
                        // (`session::handle_command`): a new `Status` must be a
                        // compile error here, not a silent "treat it as idle".
                        Command::Toggle => Some(match ctrl.status() {
                            Status::Watching | Status::Paused => Event::Stop,
                            Status::Idle | Status::Stopped(_) => Event::Start { now_ms },
                        }),
                        Command::SetFilter(filter) => Some(Event::FilterChanged(filter)),
                        Command::SetLimits(limits) => Some(Event::LimitsChanged(limits)),
                        // Timings drive the actuator, not the controller: no
                        // domain event in this preview.
                        Command::SetTimings(_) => None,
                    };
                    if let Some(event) = event {
                        // Dropped for the same reason as the seeding above.
                        let _ = ctrl.handle(event);
                    }
                    gate.set(matches!(ctrl.status(), Status::Watching | Status::Paused));
                }
                Err(mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(40))
                }
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
    });

    let error = SessionErrorSlot::default();
    eframe::run_native(
        "Arkyve Refresh Shop — UI preview",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([720.0, 680.0])
                .with_min_inner_size([520.0, 480.0]),
            ..Default::default()
        },
        Box::new(move |cc| {
            Ok(Box::new(ShopApp::new(
                cc,
                handles,
                error,
                Default::default(),
                // scratch path: the design-mock preview must never overwrite the real config.toml on Apply
                "ui_preview_scratch.toml".into(),
            )))
        }),
    )
}
