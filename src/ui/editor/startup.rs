//! Startup: the settings a running session never reads again — the game port
//! and the actuator's two mode switches — plus the notice that says so.
//!
//! # Why this section is not like the other three
//!
//! Hunt, Stop and Click timing retune a live session: their Apply emits a
//! `Command`, the session takes it, and the change is in effect before the
//! player lets go of the mouse. None of the three below can do that, and the
//! reason is different for each:
//!
//! - `game_port` builds the kernel BPF filter, compiled into every adapter
//!   when `PcapSource::open` runs once at startup (`capture/pcap/mod.rs`).
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

use eframe::egui;

use std::num::NonZeroU16;

use super::super::theme;
use crate::config::ActuatorBackend;

/// What the Startup drafts start from: the values *this* process was launched
/// with, read out of the config before `app::setup` consumes it.
///
/// A struct rather than three parameters because they travel together through
/// four call sites and mean one thing — "what a restart would currently do".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartupSettings {
    pub game_port: NonZeroU16,
    pub dry_run: bool,
    pub backend: ActuatorBackend,
}

impl Default for StartupSettings {
    /// Taken from [`crate::config::Config`]'s own `Default` rather than
    /// restated, so the drafts a
    /// preview or a test starts from cannot drift away from the ones a player
    /// with no config.toml gets. Pinned by
    /// `the_defaults_are_the_loader_s_defaults`.
    fn default() -> Self {
        let config = crate::config::Config::default();
        Self {
            game_port: config.game_port,
            dry_run: config.actuator.dry_run,
            backend: config.actuator.backend,
        }
    }
}

/// The port field's domain, and it is the loader's:
/// [`NonZeroU16`] is `1..=65_535`.
///
/// Spelled out as a range rather than derived, because a `DragValue` needs
/// concrete bounds — and pinned to the type by
/// `the_port_field_is_bounded_by_what_the_config_accepts`, so widening one
/// without the other fails a test instead of shipping.
const PORT_MIN: u16 = 1;
const PORT_MAX: u16 = u16::MAX;

/// One-line recap for the folded Startup bar.
pub(super) fn startup_summary(port: NonZeroU16, dry_run: bool, backend: ActuatorBackend) -> String {
    let mode = if dry_run {
        "rehearsal"
    } else {
        backend_label(backend)
    };
    format!("port {port} · {mode}")
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

/// The game port, bounded to exactly what [`NonZeroU16`] accepts.
///
/// The draft is a `NonZeroU16` throughout rather than a `u16` validated on
/// Apply: a field that can *hold* 0 is a field that can write `game_port = 0`
/// to config.toml, which the next launch refuses fatally — the failure
/// `hunt::grade_value` exists to prevent one field over.
pub(super) fn port_row(ui: &mut egui::Ui, port: &mut NonZeroU16) {
    row(ui, "game port", |ui| {
        let mut raw = port.get();
        let response = ui.add(
            egui::DragValue::new(&mut raw)
                .range(PORT_MIN..=PORT_MAX)
                // The same reason `stop::duration_row` gives: clamping an
                // existing value reports the response as changed, rewriting a
                // draft nobody touched. The range cannot produce 0 anyway.
                .clamp_existing_to_range(false),
        );
        if response.changed()
            && let Some(edited) = NonZeroU16::new(raw)
        {
            *port = edited;
        }
    });
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

/// Width of the label column, so the three rows' values line up the way
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

    /// The rule this section owes, same shape as
    /// `hunt::the_grade_field_is_bounded_by_what_the_config_accepts`: widen the
    /// loader without widening the field and the tab refuses a legal port;
    /// widen the field without the loader and the tab authors a config.toml the
    /// next launch will not load.
    #[test]
    fn the_port_field_is_bounded_by_what_the_config_accepts() {
        for port in [PORT_MIN, 3333, PORT_MAX] {
            let config: Config = toml::from_str(&format!("game_port = {port}"))
                .expect("the field must not offer a port the loader refuses");
            assert_eq!(config.game_port.get(), port);
        }
        // The two just outside, and the reason the draft is a `NonZeroU16`:
        // zero is the one value in `u16` the loader rejects, and a field that
        // could produce it would write a config.toml that never loads again.
        for outside in ["0", "65536"] {
            assert!(
                toml::from_str::<Config>(&format!("game_port = {outside}")).is_err(),
                "the field must not be able to author {outside}"
            );
        }
    }

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
        assert_eq!(seeded.game_port, loaded.game_port);
        assert_eq!(seeded.dry_run, loaded.actuator.dry_run);
        assert_eq!(seeded.backend, loaded.actuator.backend);
    }

    #[test]
    fn the_summary_names_the_port_and_the_mode() {
        let port = NonZeroU16::new(3333).expect("3333 is not zero");
        assert_eq!(
            startup_summary(port, false, ActuatorBackend::Message),
            "port 3333 · posted clicks"
        );
        assert_eq!(
            startup_summary(port, false, ActuatorBackend::Input),
            "port 3333 · real cursor"
        );
        // A rehearsal sends nothing, so which backend would have sent it is not
        // what the folded bar should be reporting.
        assert_eq!(
            startup_summary(port, true, ActuatorBackend::Input),
            "port 3333 · rehearsal"
        );
    }
}
