//! Click timing: the section's summary, the copy and the per-pass reading
//! under the mode control, the inline per-action bar block, and the routine
//! baseline the estimate is built from. Works on a `Timings` value handed in
//! by the caller, needing no `EditorState`. `timing_body` and `preset_row`
//! stay in the shell because they also drive the Custom disclosure flag — see
//! `_HANDOFF.md` for the draft-grouping prerequisite that would let them move.
//!
//! [`super::timing_meter`] is this group's painting, lifted out before this
//! split; it keeps its eight compile-time ruler tripwires.

use eframe::egui;

use super::super::theme;
use super::timing_meter::{secs_range, timing_group, timing_legend};
use crate::actuator::plan::{self, TimingPreset, Timings};

/// The Click timing bar is always folded on arrival, so its peek names the
/// humanization level in force, or "Custom" once fine-tuned away from a preset.
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

/// The steady find-and-buy pass as one reading — summed baseline to
/// baseline-plus-slack, in seconds — so the player sees per-pass cost without
/// decoding eight bars. Folded into `timing_body`'s hint sentence rather than
/// shown as its own stat row.
pub(super) fn pass_estimate(t: &Timings) -> String {
    let slack = [
        t.refreshed,
        t.confirm_refresh_modal,
        t.buy_modal,
        t.purchase_resumed,
    ];
    // Both ends carry the slack, and the low one has to: `min_ms` is *forced*
    // extra, added to every single draw, so a pass with four floors set is
    // never as quick as the bare baselines. Reading the low end as
    // `ROUTINE_TOTAL_MS` alone put two contradicting numbers on one screen —
    // `timing_meter::resolved_band`, in the value column of each of these four
    // bars, has always shown `baseline + min_ms` — and the contradiction grew
    // with the setting, so the player who had most reason to trust the
    // estimate got the worst one.
    //
    // Plain sums, not `saturating_add`s: `DelayRange` carries
    // `min_ms <= max_ms <= MAX_TIMING_MS` by construction, so four of either
    // end plus the routine total cannot approach `u64::MAX`, and `lo <= hi`
    // holds term by term — `secs_range` would print a backwards band if it
    // did not.
    let lo_total: u64 = ROUTINE_TOTAL_MS + slack.iter().map(|r| r.min_ms()).sum::<u64>();
    let hi_total: u64 = ROUTINE_TOTAL_MS + slack.iter().map(|r| r.max_ms()).sum::<u64>();
    secs_range(lo_total, hi_total)
}

/// The per-action bars, revealed inline when the Custom mode segment is
/// active. The legend sits here, not up top, next to the bars it explains.
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

/// The tuned baselines a single steady find-and-buy pass strings together, in
/// click order: paid refresh, its confirm, the buy, and the resume. Shop-open
/// (once), scroll/between-buys (multi-item), and the watchdog (only on a miss)
/// sit outside this steady loop. `pass_estimate` adds each action's slack on
/// top for the high end.
const ROUTINE: [u64; 4] = [
    plan::WAIT_REFRESHED_MS,
    plan::WAIT_CONFIRM_REFRESH_MODAL_MS,
    plan::WAIT_BUY_MODAL_MS,
    plan::WAIT_PURCHASE_RESUMED_MS,
];

/// The steady pass's baseline total. A `const` rather than
/// `ROUTINE.iter().sum()` inside [`pass_estimate`] (which re-summed on every
/// frame the Setup tab painted), and gives the test a fixed target to assert
/// against.
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
        // The widest legal pair `DelayRange` can hold is the ceiling on both,
        // so the label arithmetic is pinned against that: the sum must be
        // exact (not saturated), and it must be the *four* slack steps only.
        let full = DelayRange::ceiling(plan::MAX_TIMING_MS);
        let timings = Timings {
            refreshed: full,
            buy_modal: full,
            ..Timings::default()
        };
        // Asserted against the shared const, not a re-sum of `ROUTINE`.
        assert_eq!(
            pass_estimate(&timings),
            secs_range(ROUTINE_TOTAL_MS, ROUTINE_TOTAL_MS + 2 * plan::MAX_TIMING_MS)
        );
    }

    #[test]
    fn the_per_pass_low_end_agrees_with_the_bars_above_it() {
        // One screen, two readings of the same four waits: this sentence and
        // the value column of each bar. A floor of 200 ms on the paid refresh
        // means every pass really does take 200 ms longer, and
        // `timing_meter::resolved_band` says so on that row — so the estimate
        // must not keep quoting the bare baseline as its floor.
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
        // Custom wins over the detected preset so the hint tracks the lit segment.
        assert!(mode_hint(Some(TimingPreset::Instant)).starts_with("Instant"));
        assert!(mode_hint(Some(TimingPreset::Human)).starts_with("Human"));
        assert!(mode_hint(Some(TimingPreset::Cautious)).starts_with("Cautious"));
        assert!(mode_hint(None).starts_with("Custom"));
    }
}
