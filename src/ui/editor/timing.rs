//! Click timing: the section's summary, the copy and per-pass reading under the
//! mode control, the inline bar block, and the routine baseline the estimate is
//! built from. `timing_body` and `preset_row` stay in the shell because they
//! also drive the Custom disclosure flag; [`super::timing_meter`] paints.

use eframe::egui;

use super::super::theme;
use super::timing_meter::{secs_range, timing_group, timing_legend};
use crate::actuator::plan::{self, TimingPreset, Timings};

/// The folded Click timing bar's peek: the humanization level in force, or
/// "Custom" once fine-tuned away from a preset.
pub(super) fn timing_summary(timings: &Timings) -> &'static str {
    match TimingPreset::from_timings(timings) {
        Some(preset) => preset.label(),
        None => "Custom",
    }
}

/// The one-line hint under the mode control, worded for the lit segment
/// returned by `preset_row`. `None` is Custom (bars shown).
pub(super) fn mode_hint(active: Option<TimingPreset>) -> &'static str {
    match active {
        None => "Custom exposes each click's random delay — drag a bar to tune it yourself.",
        Some(TimingPreset::Instant) => {
            "Instant runs the tuned minimums — fastest, but every click fires on the same beat."
        }
        Some(TimingPreset::Human) => {
            "Human adds a little random delay to each click, so the loop never ticks like a metronome."
        }
        Some(TimingPreset::Cautious) => {
            "Cautious adds the most random delay — slowest, and the hardest to read as a bot."
        }
    }
}

/// The steady find-and-buy pass as one reading, so the player sees per-pass
/// cost without decoding eight bars.
pub(super) fn pass_estimate(t: &Timings) -> String {
    let slack = [
        t.refreshed,
        t.confirm_refresh_modal,
        t.buy_modal,
        t.purchase_resumed,
    ];
    // The low end carries the slack too: `min_ms` is *forced* extra on every
    // draw, so quoting the bare baseline contradicted the value column of these
    // same four bars, which shows `baseline + min_ms`.
    //
    // Plain sums, not `saturating_add`s: `DelayRange` is ordered and capped by
    // construction, so neither can approach `u64::MAX` and `lo <= hi` holds
    // term by term — `secs_range` would print a backwards band otherwise.
    let lo_total: u64 = ROUTINE_TOTAL_MS + slack.iter().map(|r| r.min_ms()).sum::<u64>();
    let hi_total: u64 = ROUTINE_TOTAL_MS + slack.iter().map(|r| r.max_ms()).sum::<u64>();
    secs_range(lo_total, hi_total)
}

/// The per-action bars, revealed inline when the Custom mode segment is
/// active.
pub(super) fn fine_tune_body(ui: &mut egui::Ui, t: &mut Timings) {
    timing_legend(ui);
    ui.add_space(theme::SP_SM);
    timing_group(
        ui,
        "Open & refresh",
        &mut [
            ("shop opens", &mut t.shop_opened, plan::WAIT_SHOP_OPENED_MS),
            ("paid refresh", &mut t.refreshed, plan::WAIT_REFRESHED_MS),
            (
                "confirm refresh",
                &mut t.confirm_refresh_modal,
                plan::WAIT_CONFIRM_REFRESH_MODAL_MS,
            ),
        ],
    );
    timing_group(
        ui,
        "Buy",
        &mut [
            ("confirm buy", &mut t.buy_modal, plan::WAIT_BUY_MODAL_MS),
            (
                "between buys",
                &mut t.between_buys,
                plan::WAIT_BETWEEN_BUYS_MS,
            ),
            (
                "after a scroll",
                &mut t.scroll_settle,
                plan::WAIT_SCROLL_SETTLE_MS,
            ),
            (
                "after a purchase",
                &mut t.purchase_resumed,
                plan::WAIT_PURCHASE_RESUMED_MS,
            ),
        ],
    );
    timing_group(
        ui,
        "Recovery",
        &mut [("watchdog re-issue", &mut t.recovery, plan::WAIT_RECOVERY_MS)],
    );
}

/// The tuned baselines a steady find-and-buy pass strings together. Shop-open,
/// scroll/between-buys, and the watchdog sit outside that loop.
const ROUTINE: [u64; 4] = [
    plan::WAIT_REFRESHED_MS,
    plan::WAIT_CONFIRM_REFRESH_MODAL_MS,
    plan::WAIT_BUY_MODAL_MS,
    plan::WAIT_PURCHASE_RESUMED_MS,
];

/// The steady pass's baseline total. A `const` rather than
/// `ROUTINE.iter().sum()` inside [`pass_estimate`], which re-summed on every
/// frame the Setup tab painted.
const ROUTINE_TOTAL_MS: u64 = {
    let mut total = 0;
    let mut index = 0;
    while index < ROUTINE.len() {
        total += ROUTINE[index];
        index += 1;
    }
    total
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actuator::plan::DelayRange;

    #[test]
    fn pass_estimate_tops_out_at_the_widest_range_the_type_allows() {
        // At the widest legal pair, the sum must still be exact rather than
        // saturated, over the four slack steps only.
        let full = DelayRange::ceiling(plan::MAX_TIMING_MS);
        let timings = Timings {
            refreshed: full,
            buy_modal: full,
            ..Timings::default()
        };
        assert_eq!(
            pass_estimate(&timings),
            secs_range(ROUTINE_TOTAL_MS, ROUTINE_TOTAL_MS + 2 * plan::MAX_TIMING_MS)
        );
    }

    #[test]
    fn the_per_pass_low_end_agrees_with_the_bars_above_it() {
        // Two readings of the same four waits on one screen: a 200 ms floor
        // means every pass really does take 200 ms longer.
        let timings = Timings {
            refreshed: DelayRange::try_new(200, 800).expect("a valid fixture range"),
            buy_modal: DelayRange::try_new(50, 50).expect("a valid fixture range"),
            ..Timings::default()
        };
        assert_eq!(
            pass_estimate(&timings),
            secs_range(ROUTINE_TOTAL_MS + 250, ROUTINE_TOTAL_MS + 850)
        );
    }

    #[test]
    fn mode_hint_varies_with_the_active_mode() {
        assert!(mode_hint(Some(TimingPreset::Instant)).starts_with("Instant"));
        assert!(mode_hint(Some(TimingPreset::Human)).starts_with("Human"));
        assert!(mode_hint(Some(TimingPreset::Cautious)).starts_with("Cautious"));
        assert!(mode_hint(None).starts_with("Custom"));
    }
}
