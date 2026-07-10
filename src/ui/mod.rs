//! egui window over the session: copies the controller state once per frame,
//! renders it, and pushes player commands through the existing channel. No
//! egui type crosses into the app layer or the domain.

mod editor;
mod view;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;

use crate::app::{Command, SessionHandles};
use crate::journal::LogLine;

use editor::EditorState;
use view::{ViewState, view_state};

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

    clicked = editor::edit_filter(ui, editor).or(clicked);
    clicked = editor::edit_limits(ui, editor).or(clicked);
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
