//! Click timing: the section's summary, the copy and the per-pass reading that
//! go under the mode control, the inline per-action bar block, and the routine
//! baseline the estimate is built from. A seam by shell state, not by topic:
//! everything here works on a `Timings` value handed in by the caller, so none
//! of it needs `EditorState`. `timing_body` and `preset_row` stay in the shell
//! because they also drive the Custom disclosure flag — see `_HANDOFF.md` for
//! the draft-grouping prerequisite that would let them move here.
//!
//! [`super::timing_meter`] is this group's painting, already lifted out before
//! this split; it sits beside this file and keeps its eight compile-time ruler
//! tripwires.

use eframe::egui;

use super::super::theme;
use super::timing_meter::{secs_range, timing_group, timing_legend};
use crate::actuator::plan::{self, TimingPreset, Timings};

/// The Click timing bar is always folded on arrival, so its peek names the
/// humanization level in force — the same word the mode control shows — or
/// "Custom" once the player fine-tuned away from every preset.
pub(super) fn timing_summary(timings: &Timings) -> &'static str {
    match TimingPreset::from_timings(timings) {
        Some(preset) => preset.label(),
        None => "Custom",
    }
}

/// The one-line hint under the mode control, worded for the lit segment (from
/// `preset_row`) — so the copy describes the current choice instead of listing
/// them all. `None` is Custom (bars shown).
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

/// The steady find-and-buy pass as a single honest reading — the summed baseline
/// to baseline-plus-slack, in seconds — so the player sees the loop's per-pass
/// cost without decoding eight bars. Folded into the mode hint sentence by
/// `timing_body` rather than shown as its own stat row.
pub(super) fn pass_estimate(t: &Timings) -> String {
    let slack = [
        t.refreshed,
        t.confirm_refresh_modal,
        t.buy_modal,
        t.purchase_resumed,
    ];
    // A plain sum, where this used to fold with `saturating_add` over
    // `max_ms.max(min_ms)`: `DelayRange` now carries
    // `min_ms <= max_ms <= MAX_TIMING_MS` by construction, so `max_ms` is the
    // band's top without a second look at `min_ms`, and four of them plus the
    // routine total cannot come near `u64::MAX`.
    let hi_total: u64 = ROUTINE_TOTAL_MS + slack.iter().map(|r| r.max_ms()).sum::<u64>();
    secs_range(ROUTINE_TOTAL_MS, hi_total)
}

/// The per-action bars, revealed inline when the Custom mode segment is active.
/// The legend rides here (not up top) so its two-tone key sits next to the bars
/// it explains, and the presets carry the common case above.
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
/// click order: the paid refresh, its confirm, the buy (the wait before its
/// confirm), and the resume. Shop-open (once), scroll / between-buys
/// (multi-item) and the watchdog (only on a miss) sit outside this steady loop,
/// so the summary stays an honest "typical pass". `pass_estimate` adds each
/// action's dialled-in slack on top for the high end.
const ROUTINE: [u64; 4] = [
    plan::WAIT_REFRESHED_MS,
    plan::WAIT_CONFIRM_REFRESH_MODAL_MS,
    plan::WAIT_BUY_MODAL_MS,
    plan::WAIT_PURCHASE_RESUMED_MS,
];

/// The steady pass's baseline total. A `const` rather than `ROUTINE.iter().sum()`
/// inside [`pass_estimate`], which re-summed four compile-time constants on every
/// frame the Setup tab painted — and gives the test something to assert the
/// estimate against instead of repeating the sum.
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
        // This was `pass_estimate_saturates_on_an_unvalidated_timing`, built from
        // two `max_ms = u64::MAX` ranges — a value `DelayRange` can no longer
        // hold. The widest legal pair is the ceiling on both, so that is what the
        // label arithmetic is pinned against instead: the sum must be exact, not
        // saturated, and it must be the *four* slack steps and no others.
        let full = DelayRange::ceiling(plan::MAX_TIMING_MS);
        let timings = Timings {
            refreshed: full,
            buy_modal: full,
            ..Timings::default()
        };
        // Asserted against the shared const, not a re-sum of `ROUTINE`: the two
        // used to be independent copies of the same arithmetic.
        assert_eq!(
            pass_estimate(&timings),
            secs_range(ROUTINE_TOTAL_MS, ROUTINE_TOTAL_MS + 2 * plan::MAX_TIMING_MS)
        );
    }

    #[test]
    fn mode_hint_varies_with_the_active_mode() {
        // Each mode gets its own line; Custom wins over the detected preset so
        // the hint tracks the segment lit in the control.
        assert!(mode_hint(Some(TimingPreset::Instant)).starts_with("Instant"));
        assert!(mode_hint(Some(TimingPreset::Human)).starts_with("Human"));
        assert!(mode_hint(Some(TimingPreset::Cautious)).starts_with("Cautious"));
        assert!(mode_hint(None).starts_with("Custom"));
    }
}
