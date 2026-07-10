//! egui window over the session: copies the controller state once per frame,
//! renders it, and pushes player commands through the existing channel. No
//! egui type crosses into `app.rs` or the domain.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;

use crate::app::{
    Command, LogLine, SessionHandles, describe, format_item, kind_label, status_label,
};
use crate::domain::control::{Controller, Limits, Progress, Status};
use crate::domain::filter::{Filter, SubstatReq};
use crate::domain::shop::ItemKind;

/// Where the session's terminal outcome lands (fatal error, crash, or clean
/// end): written once by the spawn wrapper in `main`, shown as a banner.
pub type SessionErrorSlot = Arc<Mutex<Option<String>>>;

/// A poisoned lock means the session panicked. The view keeps rendering the
/// last state (the banner reports the crash) instead of tearing the window
/// down with a second panic.
fn lock_ignoring_poison<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Draft criteria owned by the window until Apply pushes them to the
/// session; seeded from the controller's live criteria at startup. Edits are
/// session-only — config.toml is never rewritten.
pub struct EditorState {
    filter: Filter,
    limits: Limits,
    name_input: String,
    set_input: String,
    substat_input: String,
}

impl EditorState {
    pub fn new(filter: Filter, limits: Limits) -> Self {
        Self {
            filter,
            limits,
            name_input: String::new(),
            set_input: String::new(),
            substat_input: String::new(),
        }
    }
}

/// Plain per-frame copy of everything the window shows; built under the
/// controller lock, rendered after the guard is dropped.
pub struct ViewState {
    pub status: &'static str,
    pub stop_reason: Option<&'static str>,
    pub capture_on: bool,
    pub progress: Progress,
    pub limits: Limits,
    pub merchant: String,
    /// From the controller's enforced meta (debited per advised refresh,
    /// cleared on restart) — not the raw snapshot, which can be stale.
    pub crystal_balance: Option<u32>,
    /// Always known: wire meta, else the game constant.
    pub refresh_cost: u32,
    pub rows: Vec<SlotRow>,
}

/// One shop slot as the table shows it.
pub struct SlotRow {
    pub slot: u8,
    pub kind: &'static str,
    pub name: Option<String>,
    pub price: Option<u32>,
    pub sold_out: bool,
    /// Matched and still to buy: the catalog id sits in the checklist.
    pub wanted: bool,
    /// Full console line for the item, shown as the hover tooltip.
    pub detail: String,
}

/// Pure extraction: the caller holds the controller lock only for this call.
pub fn view_state(controller: &Controller, capture_on: bool) -> ViewState {
    let stop_reason = match controller.status() {
        Status::Stopped(reason) => Some(describe(reason)),
        _ => None,
    };
    let checklist = controller.checklist();
    let snapshot = controller.last_snapshot();
    let rows = snapshot
        .map(|snapshot| {
            snapshot
                .slots
                .iter()
                .enumerate()
                .map(|(index, item)| SlotRow {
                    slot: item.effective_slot(index),
                    kind: kind_label(item.kind),
                    name: item.name.clone(),
                    price: item.price,
                    sold_out: item.is_sold_out(),
                    wanted: item.catalog_id().is_some_and(|id| checklist.contains(&id)),
                    detail: format_item(item),
                })
                .collect()
        })
        .unwrap_or_default();
    ViewState {
        status: status_label(controller),
        stop_reason,
        capture_on,
        progress: controller.progress(),
        limits: controller.limits().clone(),
        merchant: snapshot
            .and_then(|snapshot| snapshot.merchant.clone())
            .unwrap_or_else(|| "Secret Shop".to_owned()),
        crystal_balance: controller.refresh_meta().map(|meta| meta.crystal_balance),
        refresh_cost: controller.refresh_cost(),
        rows,
    }
}

/// The eframe application: a thin shell around the session handles.
pub struct ShopApp {
    handles: SessionHandles,
    error: SessionErrorSlot,
    editor: EditorState,
}

impl ShopApp {
    pub fn new(handles: SessionHandles, error: SessionErrorSlot) -> Self {
        // Seed the drafts from the controller itself — the single source of
        // the criteria actually running.
        let editor = {
            let ctrl = lock_ignoring_poison(&handles.controller);
            EditorState::new(ctrl.filter().clone(), ctrl.limits().clone())
        };
        Self {
            handles,
            error,
            editor,
        }
    }
}

impl eframe::App for ShopApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Poll-based repaint: state changes arrive from the session loop at
        // human pace; 4 Hz keeps the window fresh without coupling app.rs to
        // egui.
        ui.ctx().request_repaint_after(Duration::from_millis(250));
        let view = {
            let ctrl = lock_ignoring_poison(&self.handles.controller);
            view_state(&ctrl, self.handles.gate.is_enabled())
        };
        let entries = self.handles.journal.entries();
        let outcome = lock_ignoring_poison(&self.error).clone();
        // Root scroll: expanded editors must never push the table or journal
        // out of a clipped (non-scrolling) panel.
        let clicked = egui::CentralPanel::default()
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("root")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        render(ui, &view, &entries, outcome.as_deref(), &mut self.editor)
                    })
                    .inner
            })
            .inner;
        if let Some(command) = clicked {
            // A full channel only happens with a dead session loop, where the
            // banner already explains the situation: dropping the click is fine.
            let _ = self.handles.commands.try_send(command);
        }
    }
}

/// Renders one frame; returns the command the player clicked, if any.
fn render(
    ui: &mut egui::Ui,
    view: &ViewState,
    journal: &[LogLine],
    outcome: Option<&str>,
    editor: &mut EditorState,
) -> Option<Command> {
    let mut clicked = None;

    if let Some(outcome) = outcome {
        ui.colored_label(ui.visuals().error_fg_color, outcome);
        ui.separator();
    }

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(view.status).strong());
        if let Some(reason) = view.stop_reason {
            ui.label(format!("({reason})"));
        }
        ui.separator();
        ui.label(if view.capture_on {
            "capture on"
        } else {
            "capture off"
        });
    });

    ui.horizontal(|ui| {
        if ui.button("Start").clicked() {
            clicked = Some(Command::Start);
        }
        if ui.button("Stop").clicked() {
            clicked = Some(Command::Stop);
        }
        if ui.button("Toggle").clicked() {
            clicked = Some(Command::Toggle);
        }
    });
    ui.separator();

    ui.horizontal(|ui| {
        ui.label(format!(
            "refreshes {}",
            against(view.progress.refreshes, view.limits.max_refreshes)
        ));
        ui.label(format!(
            "spent {}",
            against(view.progress.spent, view.limits.max_spend)
        ));
        ui.label(format!(
            "matches {}",
            against(view.progress.matches_found, view.limits.max_matches)
        ));
        ui.separator();
        let balance = match view.crystal_balance {
            Some(balance) => balance.to_string(),
            None => "—".to_owned(),
        };
        ui.label(format!(
            "crystals {balance} (refresh costs {})",
            view.refresh_cost
        ));
    });
    ui.separator();

    ui.collapsing("Filter", |ui| {
        ui.horizontal(|ui| {
            // All four kinds, Unknown included: a config-seeded criterion must
            // always be visible and clearable. Labels come from `kind_label`.
            for kind in [
                ItemKind::Equipment,
                ItemKind::Hero,
                ItemKind::Token,
                ItemKind::Unknown,
            ] {
                let mut on = editor.filter.kinds.contains(&kind);
                if ui.checkbox(&mut on, kind_label(kind)).changed() {
                    if on {
                        editor.filter.kinds.push(kind);
                    } else {
                        editor.filter.kinds.retain(|kept| *kept != kind);
                    }
                }
            }
        });
        string_list(
            ui,
            "names (exact internal ids)",
            &mut editor.filter.names,
            &mut editor.name_input,
        );
        string_list(
            ui,
            "sets (exact internal ids)",
            &mut editor.filter.sets,
            &mut editor.set_input,
        );
        substat_reqs(
            ui,
            &mut editor.filter.required_substats,
            &mut editor.substat_input,
        );
        optional_value(ui, "min substats", &mut editor.filter.min_substats, 1);
        // Seeded above the covenant-bookmark price so a fresh cap still
        // matches the default hunt targets.
        optional_value(
            ui,
            "max price (gold)",
            &mut editor.filter.max_price,
            300_000,
        );
        ui.checkbox(&mut editor.filter.include_sold_out, "include sold out");
        let restricted = !editor.filter.is_unrestricted();
        if !restricted {
            ui.weak("at least one criterion is required before Apply");
        }
        if ui
            .add_enabled(restricted, egui::Button::new("Apply filter"))
            .clicked()
        {
            clicked = Some(Command::SetFilter(editor.filter.clone()));
        }
    });
    ui.collapsing("Limits", |ui| {
        optional_value(ui, "max refreshes", &mut editor.limits.max_refreshes, 10);
        optional_value(ui, "max spend (crystals)", &mut editor.limits.max_spend, 30);
        optional_value(ui, "max matches", &mut editor.limits.max_matches, 5);
        duration_minutes(ui, &mut editor.limits.max_duration_ms);
        if ui.button("Apply limits").clicked() {
            clicked = Some(Command::SetLimits(editor.limits.clone()));
        }
    });
    ui.weak("edits apply to this session only — config.toml is unchanged");
    ui.separator();

    ui.label(egui::RichText::new(view.merchant.as_str()).strong());
    if view.rows.is_empty() {
        ui.weak("no shop captured yet — open the Secret Shop in game");
    } else {
        egui::Grid::new("shop")
            .num_columns(4)
            .striped(true)
            .show(ui, |ui| {
                ui.strong("Slot");
                ui.strong("Kind");
                ui.strong("Name");
                ui.strong("Price");
                ui.end_row();
                for row in &view.rows {
                    let cell = |text: String| {
                        let mut text = egui::RichText::new(text);
                        if row.wanted {
                            text = text.strong().color(egui::Color32::LIGHT_GREEN);
                        }
                        if row.sold_out {
                            text = text.weak().strikethrough();
                        }
                        text
                    };
                    ui.label(cell(row.slot.to_string()));
                    ui.label(cell(row.kind.to_owned()));
                    ui.label(cell(row.name.clone().unwrap_or_else(|| "—".to_owned())))
                        .on_hover_text(&row.detail);
                    ui.label(cell(match row.price {
                        Some(price) => format!("{price} gold"),
                        None => "—".to_owned(),
                    }));
                    ui.end_row();
                }
            });
    }
    ui.separator();

    ui.label(egui::RichText::new("Journal").strong());
    egui::ScrollArea::vertical()
        .id_salt("journal")
        .stick_to_bottom(true)
        .max_height(180.0)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for line in journal {
                ui.monospace(format!("[{}] {}", timestamp(line.at_ms), line.text));
            }
        });

    clicked
}

/// One editable any-of list: entries with a remove cross plus an add row.
fn string_list(ui: &mut egui::Ui, label: &str, values: &mut Vec<String>, input: &mut String) {
    ui.label(label);
    let mut removed = None;
    for (index, value) in values.iter().enumerate() {
        // Stable per-row ids: without them a removal shifts the positional
        // auto-ids of every widget below (focus, edit state).
        ui.push_id(index, |ui| {
            ui.horizontal(|ui| {
                ui.monospace(value);
                if ui.small_button("✕").clicked() {
                    removed = Some(index);
                }
            });
        });
    }
    if let Some(index) = removed {
        values.remove(index);
    }
    ui.horizontal(|ui| {
        ui.text_edit_singleline(input);
        if ui.button("add").clicked() && !input.trim().is_empty() {
            values.push(input.trim().to_owned());
            input.clear();
        }
    });
}

/// Required-substat rows: name, optional min threshold, remove cross.
fn substat_reqs(ui: &mut egui::Ui, reqs: &mut Vec<SubstatReq>, input: &mut String) {
    ui.label("required substats");
    let mut removed = None;
    for (index, req) in reqs.iter_mut().enumerate() {
        ui.push_id(index, |ui| {
            ui.horizontal(|ui| {
                ui.monospace(&req.name);
                let mut has_min = req.min.is_some();
                ui.checkbox(&mut has_min, "min");
                if has_min {
                    ui.add(egui::DragValue::new(req.min.get_or_insert(1.0)).speed(0.5));
                } else {
                    req.min = None;
                }
                if ui.small_button("✕").clicked() {
                    removed = Some(index);
                }
            });
        });
    }
    if let Some(index) = removed {
        reqs.remove(index);
    }
    ui.horizontal(|ui| {
        ui.text_edit_singleline(input);
        if ui.button("add").clicked() && !input.trim().is_empty() {
            reqs.push(SubstatReq {
                name: input.trim().to_owned(),
                min: None,
            });
            input.clear();
        }
    });
}

/// Checkbox-gated numeric criterion: unchecked means "no constraint". The
/// seed must be a sensible non-zero value — a zero limit halts the session at
/// the next check-point and a zero criterion constrains nothing.
fn optional_value<T: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut Option<T>,
    seed: T,
) {
    ui.horizontal(|ui| {
        let mut on = value.is_some();
        ui.checkbox(&mut on, label);
        if on {
            ui.add(egui::DragValue::new(value.get_or_insert(seed)));
        } else {
            *value = None;
        }
    });
}

/// The duration limit, edited in whole minutes (stored as ms).
fn duration_minutes(ui: &mut egui::Ui, value: &mut Option<u64>) {
    ui.horizontal(|ui| {
        let mut on = value.is_some();
        ui.checkbox(&mut on, "max duration (minutes)");
        if on {
            let ms = value.get_or_insert(60 * 60_000);
            // Ceil so a sub-minute config value never reads as 0; edits are
            // whole minutes (the player-facing unit) and only rewrite the
            // stored value when the player actually drags.
            let mut minutes = ms.div_ceil(60_000);
            if ui.add(egui::DragValue::new(&mut minutes)).changed() {
                *ms = minutes.saturating_mul(60_000);
            }
        } else {
            *value = None;
        }
    });
}

/// `3/10` against a limit, `3/—` without one.
fn against(value: u32, limit: Option<u32>) -> String {
    match limit {
        Some(limit) => format!("{value}/{limit}"),
        None => format!("{value}/—"),
    }
}

/// Session-relative `+m:ss` (hours appear once the session runs that long).
fn timestamp(at_ms: u64) -> String {
    let secs = at_ms / 1000;
    let (hours, minutes, seconds) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if hours > 0 {
        format!("+{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("+{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::control::Event;
    use crate::domain::filter::Filter;
    use crate::domain::shop::{PurchaseLimit, RefreshMeta, ShopItem, ShopSnapshot};

    fn controller() -> Controller {
        Controller::new(Filter::default(), Limits::default())
    }

    fn shop(slots: Vec<ShopItem>) -> ShopSnapshot {
        ShopSnapshot {
            merchant: None,
            slots,
            refresh: None,
        }
    }

    #[test]
    fn view_state_on_fresh_controller_is_idle_and_empty() {
        let view = view_state(&controller(), false);
        assert!(view.status.contains("idle"));
        assert_eq!(view.stop_reason, None);
        assert!(!view.capture_on);
        assert!(view.rows.is_empty());
        assert_eq!(view.merchant, "Secret Shop");
        assert_eq!(view.crystal_balance, None);
        // No meta yet: the game-constant fallback, never an unknown cost.
        assert_eq!(view.refresh_cost, 3);
    }

    #[test]
    fn view_state_passes_capture_flag_through() {
        assert!(view_state(&controller(), true).capture_on);
        assert!(!view_state(&controller(), false).capture_on);
    }

    #[test]
    fn view_state_rows_use_effective_slot_fallback() {
        let mut ctrl = controller();
        // First slot carries a wire slot, second falls back to its 1-based
        // position. Stored while Idle: storage does not require an armed loop.
        let slots = vec![
            ShopItem {
                slot: 5,
                ..ShopItem::default()
            },
            ShopItem {
                slot: 0,
                ..ShopItem::default()
            },
        ];
        ctrl.handle(Event::Snapshot {
            snapshot: shop(slots),
            now_ms: 0,
        });
        let view = view_state(&ctrl, false);
        assert_eq!(view.rows[0].slot, 5);
        assert_eq!(view.rows[1].slot, 2);
    }

    #[test]
    fn view_state_flags_checklist_rows_as_wanted() {
        let mut ctrl = controller();
        ctrl.handle(Event::Start { now_ms: 0 });
        // Default filter matches both; only the trackable id enters the
        // checklist — the id-0 sentinel row must never read as wanted.
        let slots = vec![
            ShopItem {
                id: 42,
                ..ShopItem::default()
            },
            ShopItem {
                id: 0,
                ..ShopItem::default()
            },
        ];
        ctrl.handle(Event::Snapshot {
            snapshot: shop(slots),
            now_ms: 1,
        });
        let view = view_state(&ctrl, true);
        assert!(view.rows[0].wanted);
        assert!(!view.rows[1].wanted);
    }

    #[test]
    fn view_state_flags_sold_out_rows() {
        let mut ctrl = controller();
        let slots = vec![ShopItem {
            limit: Some(PurchaseLimit {
                remaining: 0,
                total: 1,
            }),
            ..ShopItem::default()
        }];
        ctrl.handle(Event::Snapshot {
            snapshot: shop(slots),
            now_ms: 0,
        });
        assert!(view_state(&ctrl, false).rows[0].sold_out);
    }

    #[test]
    fn view_state_copies_refresh_meta_when_present() {
        let mut ctrl = controller();
        let snapshot = ShopSnapshot {
            merchant: Some("Secret Shop".to_owned()),
            slots: vec![ShopItem::default()],
            refresh: Some(RefreshMeta {
                crystal_balance: 95,
                cost: 3,
            }),
        };
        ctrl.handle(Event::Snapshot {
            snapshot,
            now_ms: 0,
        });
        let view = view_state(&ctrl, false);
        assert_eq!(view.merchant, "Secret Shop");
        assert_eq!(view.crystal_balance, Some(95));
        assert_eq!(view.refresh_cost, 3);
    }

    #[test]
    fn view_state_balance_survives_meta_less_snapshot() {
        // The controller keeps its enforced estimate across snapshots that
        // omit meta; the display must show that, not the raw snapshot.
        let mut ctrl = controller();
        ctrl.handle(Event::Snapshot {
            snapshot: ShopSnapshot {
                merchant: None,
                slots: vec![ShopItem::default()],
                refresh: Some(RefreshMeta {
                    crystal_balance: 95,
                    cost: 3,
                }),
            },
            now_ms: 0,
        });
        ctrl.handle(Event::Snapshot {
            snapshot: shop(vec![ShopItem::default()]),
            now_ms: 1,
        });
        assert_eq!(view_state(&ctrl, false).crystal_balance, Some(95));
    }

    #[test]
    fn view_state_balance_cleared_on_restart() {
        // `Start` discards a stale balance; the display must not resurrect it
        // from the stored snapshot.
        let mut ctrl = controller();
        ctrl.handle(Event::Snapshot {
            snapshot: ShopSnapshot {
                merchant: None,
                slots: vec![ShopItem::default()],
                refresh: Some(RefreshMeta {
                    crystal_balance: 95,
                    cost: 3,
                }),
            },
            now_ms: 0,
        });
        ctrl.handle(Event::Start { now_ms: 1 });
        assert_eq!(view_state(&ctrl, false).crystal_balance, None);
    }

    #[test]
    fn view_state_reports_stop_reason_when_stopped() {
        let mut ctrl = controller();
        ctrl.handle(Event::Start { now_ms: 0 });
        ctrl.handle(Event::Stop);
        let view = view_state(&ctrl, false);
        assert!(view.status.contains("stopped"));
        assert_eq!(view.stop_reason, Some("player stopped"));
    }

    #[test]
    fn view_state_detail_matches_format_item() {
        let mut ctrl = controller();
        let item = ShopItem {
            id: 7,
            slot: 3,
            name: Some("Covenant Bookmark".to_owned()),
            price: Some(184_000),
            ..ShopItem::default()
        };
        let expected = format_item(&item);
        ctrl.handle(Event::Snapshot {
            snapshot: shop(vec![item]),
            now_ms: 0,
        });
        assert_eq!(view_state(&ctrl, false).rows[0].detail, expected);
    }

    #[test]
    fn against_renders_missing_limit_as_dash() {
        assert_eq!(against(3, Some(10)), "3/10");
        assert_eq!(against(3, None), "3/—");
    }

    #[test]
    fn timestamp_rolls_into_hours() {
        assert_eq!(timestamp(59_000), "+0:59");
        assert_eq!(timestamp(61_000), "+1:01");
        assert_eq!(timestamp(3_661_000), "+1:01:01");
    }
}
