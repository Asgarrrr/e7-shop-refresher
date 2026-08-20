//! Stop: the run's safety rails — the section's summary and the ledger-row
//! widgets they're drawn from (shared row chrome, the two rail kinds, the
//! compact value chip). Works on a `Limits` field handed in by the caller,
//! needing no `EditorState`. `stop_body` stays in the shell — see
//! `_HANDOFF.md` for the draft-grouping prerequisite that would let it move.
//!
//! Arming and the shared drag field stay in the shell ([`super::arm_optional`],
//! [`super::optional_field`]) since Hunt's grid cells route through the same
//! two, keeping the `clamp_existing_to_range(false)` fix single-site.

use eframe::egui;

use super::super::theme;
use super::{arm_optional, count_label, optional_field};
use crate::domain::control::Limits;

/// One-line recap of the active stop limits for the folded Stop bar; "no limits"
/// when the run is uncapped.
pub(super) fn stop_summary(limits: &Limits) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(n) = limits.max_refreshes {
        parts.push(count_label(
            usize::try_from(n).unwrap_or(usize::MAX),
            "refresh",
            "refreshes",
        ));
    }
    if let Some(n) = limits.max_spend {
        parts.push(format!("{n} crystals"));
    }
    if let Some(n) = limits.max_matches {
        parts.push(count_label(
            usize::try_from(n).unwrap_or(usize::MAX),
            "match",
            "matches",
        ));
    }
    if let Some(ms) = limits.max_duration_ms {
        parts.push(format!("{} min", ms.div_ceil(60_000)));
    }
    if parts.is_empty() {
        "no limits".to_owned()
    } else {
        parts.join(" · ")
    }
}

/// One ledger row: `☑ unit …… value` — checkbox arms the cap, unit in the left
/// column, value flush-right in its own column. Arming is [`arm_optional`]'s,
/// the field is [`optional_field`]'s; an unset rail reads a faint "none".
pub(super) fn limit_row<T: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    unit: &str,
    value: &mut Option<T>,
    seed: T,
) {
    limit_ledger_row(ui, unit, value.is_some(), |on, ui| {
        arm_optional(on, value, seed);
        if let Some(current) = value.as_mut() {
            compact_drag(ui, |ui| {
                optional_field(ui, current);
            });
        } else {
            ui.colored_label(theme::INK_FAINT, "none");
        }
    });
}

/// The duration rail, edited in whole minutes (stored as ms) — a [`limit_row`]
/// twin kept apart for its minute↔ms conversion. Arming is shared
/// ([`arm_optional`]); the field is not, because it drags a derived value.
pub(super) fn duration_row(ui: &mut egui::Ui, value: &mut Option<u64>) {
    limit_ledger_row(ui, "minutes", value.is_some(), |on, ui| {
        arm_optional(on, value, 60 * 60_000);
        if let Some(ms) = value {
            // Ceil so a sub-minute config value never reads as 0; only a real
            // drag rewrites the stored ms.
            let mut minutes = ms.div_ceil(60_000);
            compact_drag(ui, |ui| {
                // `clamp_existing_to_range(false)` for the same reason
                // [`super::optional_field`] carries it, and this row needed it
                // too: `max_duration_ms = Some(0)` is a legal config value
                // (nothing in `config` rejects it, and `Controller` reads it as
                // "stop at once"), it renders as `minutes = 0`, and egui's
                // default would clamp that existing value up to the range floor
                // and report the response as *changed* on the first frame. The
                // handler below would then write 60_000 ms into a draft the
                // player has not touched, lighting Apply with a one-minute
                // session cap nobody asked for.
                let r = ui.add(
                    egui::DragValue::new(&mut minutes)
                        .range(1..=u64::MAX / 60_000)
                        .clamp_existing_to_range(false),
                );
                if r.changed() {
                    *ms = minutes.saturating_mul(60_000);
                }
            });
        } else {
            ui.colored_label(theme::INK_FAINT, "none");
        }
    });
}

/// The shared ledger-row chrome: arming checkbox and unit label on the left,
/// the caller's value flush-right in its own column. `armed` seeds the
/// checkbox and the unit's ink (muted when live, faint when off); `value`
/// paints the right column after the toggle resolves.
fn limit_ledger_row(
    ui: &mut egui::Ui,
    unit: &str,
    armed: bool,
    value: impl FnOnce(bool, &mut egui::Ui),
) {
    ui.horizontal(|ui| {
        let mut on = armed;
        theme::accent_checkbox(ui, &mut on);
        let color = if on {
            theme::INK_MUTED
        } else {
            theme::INK_FAINT
        };
        // Fixed-width label cell so the value column starts at the same x on
        // every row, without flushing to the far edge (which opened a dead gap).
        let (rect, _) = ui.allocate_exact_size(egui::vec2(130.0, 20.0), egui::Sense::hover());
        ui.painter().with_clip_rect(rect).text(
            egui::pos2(rect.left(), rect.center().y),
            egui::Align2::LEFT_CENTER,
            unit,
            egui::FontId::new(14.0, egui::FontFamily::Proportional),
            color,
        );
        value(on, ui);
    });
}

/// A compact drag field for the ledger's value column: the default filled box,
/// with tightened padding and a 3px corner so each value reads as a small chip.
fn compact_drag(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    ui.scope(|ui| {
        ui.spacing_mut().button_padding = egui::vec2(6.0, 3.0);
        let widgets = &mut ui.style_mut().visuals.widgets;
        for state in [
            &mut widgets.inactive,
            &mut widgets.hovered,
            &mut widgets.active,
        ] {
            state.corner_radius = egui::CornerRadius::same(3);
        }
        add(ui);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_summary_lists_active_limits() {
        let limits = Limits {
            max_refreshes: Some(1),
            max_matches: Some(5),
            ..Limits::default()
        };
        assert_eq!(stop_summary(&limits), "1 refresh · 5 matches");
        assert_eq!(stop_summary(&Limits::default()), "no limits");
    }
}
