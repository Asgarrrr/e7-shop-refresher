//! Clicking: whether the actuator sends real input, and which Win32 path it
//! sends it through.
//!
//! Both are live. They were not, and the difference is worth keeping because
//! the reason they became live is the reason this section is shaped the way it
//! is: `config.example.toml` calls `backend = "input"` the fallback for when a
//! game update stops honouring posted clicks, so the moment a player needs it
//! is mid-session, with the tool having just stopped working. A field that
//! answered "restart the app" at that moment was answering at the worst
//! possible time.
//!
//! # One value, not two fields
//!
//! The pair travels as a single [`ClickMode`] from here to the executor:
//! [`crate::app::Command::SetClickMode`] carries both, the shared cell holds
//! both, and the executor snapshots both at once. A single Apply must not be
//! able to land in halves — a job running the old backend in the new rehearsal
//! state would engage the window and then only journal, which is the one
//! outcome neither setting is allowed to produce.
//!
//! # When a change takes effect
//!
//! On the next job the executor *dequeues*, never mid-job. That is a stronger
//! promise than the click timings beside it, which bake in at submit time: a
//! job already queued still uses the waits it was planned with, but will run in
//! whatever mode is current when it starts. Both read as "applies to the next
//! clicks" to the player; only one of them can be baked in, because a job
//! carries no surface.

use eframe::egui;

use super::super::theme;
use crate::actuator::{ActuatorBackend, ClickMode};

/// One-line recap for the folded Clicking bar.
pub(super) fn clicking_summary(mode: ClickMode) -> String {
    if mode.dry_run {
        "rehearsal".to_owned()
    } else {
        backend_label(mode.backend).to_owned()
    }
}

/// The label a player reads, per backend. Not the TOML spelling: `message`
/// says nothing about what changes, and this is the switch someone reaches for
/// when the tool has stopped working.
fn backend_label(backend: ActuatorBackend) -> &'static str {
    match backend {
        ActuatorBackend::Message => "posted clicks",
        ActuatorBackend::Input => "real cursor",
    }
}

/// One line of explanation under the switch, in the terms
/// `config.example.toml` already uses.
fn backend_hint(backend: ActuatorBackend) -> &'static str {
    match backend {
        ActuatorBackend::Message => "no focus stolen, the mouse stays yours",
        ActuatorBackend::Input => "moves the real cursor; use if posted clicks stop working",
    }
}

/// The rehearsal switch.
pub(super) fn dry_run_row(ui: &mut egui::Ui, dry_run: &mut bool) {
    row(ui, "rehearse only", |ui| {
        theme::accent_checkbox(ui, dry_run);
        ui.colored_label(
            theme::INK_FAINT,
            "plans and journals every click, sends none",
        );
    });
}

/// The two-value backend selector.
pub(super) fn backend_row(ui: &mut egui::Ui, backend: &mut ActuatorBackend) {
    row(ui, "clicks reach the game", |ui| {
        for choice in [ActuatorBackend::Message, ActuatorBackend::Input] {
            if ui
                .selectable_label(*backend == choice, backend_label(choice))
                .clicked()
            {
                *backend = choice;
            }
        }
    });
    ui.horizontal(|ui| {
        ui.add_space(LABEL_WIDTH);
        ui.colored_label(theme::INK_FAINT, backend_hint(*backend));
    });
}

/// When a pending change will bite. Replaces the restart notice this section
/// used to carry, and is not decoration: "the next clicks" is a real delay a
/// player would otherwise read as the switch having failed.
pub(super) fn timing_notice(ui: &mut egui::Ui, dirty: bool) {
    let (color, text) = if dirty {
        (
            theme::INK_MUTED,
            "Apply changes the mode from the next clicks — a job already running finishes as it started",
        )
    } else {
        (theme::INK_FAINT, "changes apply from the next clicks")
    };
    ui.colored_label(color, text);
}

/// Width of the label column, so both rows' values line up the way
/// `stop::limit_ledger_row` lines its own up.
const LABEL_WIDTH: f32 = 130.0;

/// The shared row chrome: a fixed-width label, then the caller's control.
fn row(ui: &mut egui::Ui, label: &str, value: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(LABEL_WIDTH, 20.0), egui::Sense::hover());
        ui.painter().with_clip_rect(rect).text(
            egui::pos2(rect.left(), rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::new(14.0, egui::FontFamily::Proportional),
            theme::INK_MUTED,
        );
        value(ui);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// Both switches, from the widget's side: every value the selector offers
    /// must be one the loader reads back as the same variant. The writer's half
    /// lives in `config::persist::every_backend_writes_a_value_the_loader_accepts`.
    #[test]
    fn every_offered_backend_is_one_the_config_accepts() {
        for (backend, spelling) in [
            (ActuatorBackend::Message, "message"),
            (ActuatorBackend::Input, "input"),
        ] {
            let config: Config = toml::from_str(&format!("[actuator]\nbackend = \"{spelling}\""))
                .expect("the selector must not offer a backend the loader refuses");
            assert_eq!(config.actuator.backend, backend);
        }
    }

    /// `dry_run`'s domain is `bool`, so the only thing left to pin is that the
    /// loader really does take both — a `#[serde(deserialize_with)]` added
    /// later could narrow it without anyone noticing here.
    #[test]
    fn both_rehearsal_states_are_ones_the_config_accepts() {
        for dry_run in [true, false] {
            let config: Config = toml::from_str(&format!("[actuator]\ndry_run = {dry_run}"))
                .expect("the checkbox must not offer a state the loader refuses");
            assert_eq!(config.actuator.dry_run, dry_run);
        }
    }

    /// The draft a fresh window shows must be the one a fresh config.toml would
    /// load. `ClickMode::default()` and the loader's default are two separate
    /// declarations of the same intent, and this is what stops them drifting.
    #[test]
    fn the_default_draft_is_the_loader_s_default() {
        let seeded = ClickMode::default();
        let loaded: Config = toml::from_str("").expect("an empty config is valid");
        assert_eq!(seeded.dry_run, loaded.actuator.dry_run);
        assert_eq!(seeded.backend, loaded.actuator.backend);
    }

    #[test]
    fn the_summary_names_the_mode() {
        assert_eq!(
            clicking_summary(ClickMode {
                dry_run: false,
                backend: ActuatorBackend::Message
            }),
            "posted clicks"
        );
        assert_eq!(
            clicking_summary(ClickMode {
                dry_run: false,
                backend: ActuatorBackend::Input
            }),
            "real cursor"
        );
        // A rehearsal sends nothing, so which backend would have sent it is not
        // what the folded bar should be reporting.
        assert_eq!(
            clicking_summary(ClickMode {
                dry_run: true,
                backend: ActuatorBackend::Input
            }),
            "rehearsal"
        );
    }
}
