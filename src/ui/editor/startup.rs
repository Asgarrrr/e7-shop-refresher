//! Startup: the settings a running session never reads again — the actuator's
//! two mode switches — plus the notice that says so.
//!
//! # Why this section is not like the other three
//!
//! Hunt, Stop and Click timing retune a live session: their Apply emits a
//! `Command`, the session takes it, and the change is in effect before the
//! player lets go of the mouse. Neither of the two below can do that:
//!
//! - `backend` picks the `Surface` that is *moved into* `run_executor` along
//!   with the only receiver for its job queue (`app::Session::run`).
//! - `dry_run` is read by that same executor, and separately decides whether
//!   the controller arms its recovery watchdog — a dry run produces no wire
//!   feedback, so an armed watchdog would halt blaming the game.
//!
//! So each field carries a restart notice, and that notice is part of the
//! feature rather than decoration. A setting that silently does nothing until
//! the next launch is worse than no editor at all: the player concludes the
//! tool is broken, which is exactly the state they were already in when they
//! came here.
//!
//! # `game_port` is deliberately not here
//!
//! It was, briefly (`8d25453`), and it was taken out again on the maintainer's
//! decision: the port is not something a player sets from the window. It stays
//! a `config.toml` key, and `README.md` says so rather than pointing at a field
//! that does not exist.
//!
//! Note that it *would* have been the restart-only case with the hardest
//! constraint of the three — the port is compiled into every adapter's kernel
//! BPF filter when `PcapSource::open` runs, once, at process start — so nothing
//! about that argument is lost by dropping it. If it is ever wanted back, the
//! shape is in `8d25453`.

use eframe::egui;

use super::super::theme;
use crate::config::ActuatorBackend;

/// What the Startup drafts start from: the values *this* process was launched
/// with, read out of the config before `app::setup` consumes it.
///
/// A struct rather than two parameters because they travel together through
/// four call sites and mean one thing — "what a restart would currently do".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartupSettings {
    pub dry_run: bool,
    pub backend: ActuatorBackend,
}

impl Default for StartupSettings {
    /// Taken from [`crate::config::Config`]'s own `Default` rather than
    /// restated, so the drafts a preview or a test starts from cannot drift
    /// away from the ones a player with no config.toml gets. Pinned by
    /// `the_defaults_are_the_loader_s_defaults`.
    fn default() -> Self {
        let config = crate::config::Config::default();
        Self {
            dry_run: config.actuator.dry_run,
            backend: config.actuator.backend,
        }
    }
}

/// One-line recap for the folded Startup bar.
pub(super) fn startup_summary(dry_run: bool, backend: ActuatorBackend) -> String {
    if dry_run {
        "rehearsal".to_owned()
    } else {
        backend_label(backend).to_owned()
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

/// One line of explanation under each switch, in the terms
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

/// The notice that makes the section honest. Rendered once under the three
/// rows rather than three times: they share one reason and one remedy.
pub(super) fn restart_notice(ui: &mut egui::Ui, dirty: bool) {
    let text = if dirty {
        "Apply writes these to config.toml — they take effect when you restart the app"
    } else {
        "these take effect when you restart the app"
    };
    ui.colored_label(
        if dirty {
            theme::INK_MUTED
        } else {
            theme::INK_FAINT
        },
        text,
    );
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

    /// The drafts a fresh window shows must be the ones a fresh config.toml
    /// would load. Restating them here instead of deriving them is how the two
    /// drift, and a drifted default writes a change the player never made.
    #[test]
    fn the_defaults_are_the_loader_s_defaults() {
        let seeded = StartupSettings::default();
        let loaded: Config = toml::from_str("").expect("an empty config is valid");
        assert_eq!(seeded.dry_run, loaded.actuator.dry_run);
        assert_eq!(seeded.backend, loaded.actuator.backend);
    }

    #[test]
    fn the_summary_names_the_mode() {
        assert_eq!(
            startup_summary(false, ActuatorBackend::Message),
            "posted clicks"
        );
        assert_eq!(
            startup_summary(false, ActuatorBackend::Input),
            "real cursor"
        );
        // A rehearsal sends nothing, so which backend would have sent it is not
        // what the folded bar should be reporting.
        assert_eq!(startup_summary(true, ActuatorBackend::Input), "rehearsal");
    }
}
