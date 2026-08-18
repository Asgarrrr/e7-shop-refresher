//! Stop: the run's safety rails — the section's summary and the ledger-row
//! widgets the rails are drawn from (the shared row chrome, the two rail kinds,
//! the compact value chip). A seam by shell state, not by topic: everything here
//! works on a `Limits` field handed in by the caller, so none of it needs
//! `EditorState`. `stop_body` itself still lives in the shell — see
//! `_HANDOFF.md` for the draft-grouping prerequisite that would let it move.
//!
//! The arming semantics and the shared drag field stay in the shell
//! ([`super::arm_optional`], [`super::optional_field`]) because Hunt's grid
//! cells route through the same two; keeping them one level up is what keeps
//! the `clamp_existing_to_range(false)` fix single-site.

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

/// One ledger row: `☑ unit …… value`. The checkbox arms the cap, the unit sits
/// in the left column, and the value is pushed flush-right so every cap lines up
/// in its own column. Arming is [`arm_optional`]'s, the field is
/// [`optional_field`]'s; an unset rail reads a faint "none".
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
/// twin kept apart for its minute↔ms conversion, exactly as
/// [`super::hunt::optional_value`] and the old `duration_minutes` were split
/// before. Arming is shared ([`arm_optional`]); the field is not, because it
/// drags a derived value.
pub(super) fn duration_row(ui: &mut egui::Ui, value: &mut Option<u64>) {
    limit_ledger_row(ui, "minutes", value.is_some(), |on, ui| {
        arm_optional(on, value, 60 * 60_000);
        if let Some(ms) = value {
            // Ceil so a sub-minute config value never reads as 0; edits are whole
            // minutes and only rewrite the stored ms on a real drag.
            let mut minutes = ms.div_ceil(60_000);
            compact_drag(ui, |ui| {
                let r = ui.add(egui::DragValue::new(&mut minutes).range(1..=u64::MAX / 60_000));
                if r.changed() {
                    *ms = minutes.saturating_mul(60_000);
                }
            });
        } else {
            ui.colored_label(theme::INK_FAINT, "none");
        }
    });
}

/// The shared ledger-row chrome: the arming checkbox and unit label on the left,
/// then the caller's value flush-right in its own column. `armed` seeds the
/// checkbox and the unit's ink (muted when live, faint when off); `value` paints
/// the right column after the toggle has been resolved. Splitting the chrome out
/// keeps the two rail kinds (numeric / duration) down to just their value widget.
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
        // Fixed-width label cell so the value column starts at the same x on every
        // row (aligned like a ledger) without flushing to the far edge — which
        // opened a dead gap between label and value.
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

/// A compact drag field for the ledger's value column: the default filled box —
/// so it still reads plainly as an editable input — but with tightened padding
/// and a 3px corner, so each value sits as a small chip instead of the bulky
/// default pill that would dominate the column.
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
