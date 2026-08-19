//! The timed input sequences themselves: what to click, in what order, and how
//! long to wait before each act.
//!
//! The top layer — the only module here that reads all three of the others. Each
//! builder pairs a [`geometry`](super::geometry) zone with a
//! [`timings`](super::timings) wait through two [`Jitter`] streams salted apart,
//! and hands back a [`Job`] the executor replays step by step.

use super::geometry::{
    CONFIRM_BUY, CONFIRM_REFRESH, DesignPoint, LAST_TOP_ROW, REFRESH, Row, SCROLL_ZONE, Zone,
    buy_zone,
};
use super::jitter::{DELAY_SEED_SALT, Jitter};
use super::timings::{Timings, Trigger};

/// Wheel notches for one scroll-to-extreme — generous on purpose: the list
/// clamps, so overshooting is free and the resulting position deterministic.
const SCROLL_TO_EXTREME_NOTCHES: i32 = 10;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Input {
    /// Press at `at`, hold `press_ms`, release.
    Click { at: DesignPoint, press_ms: u64 },
    /// Positive notches scroll the list up (toward the top).
    Scroll { at: DesignPoint, notches: i32 },
}

impl Input {
    pub fn at(&self) -> DesignPoint {
        match *self {
            Input::Click { at, .. } | Input::Scroll { at, .. } => at,
        }
    }
}

/// One input, preceded by the wait that makes it land outside any
/// input-blocking animation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimedStep {
    pub wait_ms: u64,
    pub input: Input,
}

/// The shop-generation number a plan was built against, read from
/// [`SnapshotEpoch::current`](crate::actuator::SnapshotEpoch::current).
///
/// A newtype because every job builder below takes it *immediately before* a
/// `seed: u64` drawn from `now_ms`, so the two used to be adjacent bare `u64`s
/// and a transposition compiled. It would not have produced a wrong click, which
/// is worse: the executor's first act on every job is `job.epoch !=
/// epoch.current()`, so a swapped pair drops *every* click forever while the
/// journal blames the shop for changing. Only ever compared for equality —
/// nothing here does arithmetic on a generation number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Epoch(pub u64);

/// A full input sequence, valid for one shop state: `epoch` is the snapshot
/// generation the plan was built against — the executor drops the job once a
/// newer shop has arrived.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub epoch: Epoch,
    pub steps: Vec<TimedStep>,
}

fn click(jitter: &mut Jitter, zone: Zone) -> Input {
    Input::Click {
        at: jitter.point_in(zone),
        press_ms: jitter.press_ms(),
    }
}

fn scroll(jitter: &mut Jitter, notches: i32) -> Input {
    Input::Scroll {
        at: jitter.point_in(SCROLL_ZONE),
        notches,
    }
}

/// One jittered click in the given confirm zone: the watchdog's free nudge
/// after a confirm click that missed its modal. Safe on the shop screen —
/// nothing clickable sits under either confirm zone when no modal is open
/// (player-confirmed game fact).
#[must_use = "a planned job that is never submitted is a lost click"]
pub fn confirm_retry_job(zone: Zone, timings: Timings, epoch: Epoch, seed: u64) -> Job {
    let mut jitter = Jitter::new(seed);
    let mut delay = Jitter::new(seed ^ DELAY_SEED_SALT);
    Job {
        epoch,
        steps: vec![TimedStep {
            wait_ms: timings.pre_wait_ms(Trigger::Recovery, &mut delay),
            input: click(&mut jitter, zone),
        }],
    }
}

/// Refresh = click Refresh, wait out the confirm modal, click its yes.
#[must_use = "a planned job that is never submitted is a lost click"]
pub fn refresh_job(trigger: Trigger, timings: Timings, epoch: Epoch, seed: u64) -> Job {
    let mut jitter = Jitter::new(seed);
    let mut delay = Jitter::new(seed ^ DELAY_SEED_SALT);
    let steps = vec![
        TimedStep {
            wait_ms: timings.pre_wait_ms(trigger, &mut delay),
            input: click(&mut jitter, REFRESH),
        },
        TimedStep {
            wait_ms: timings.confirm_refresh_modal_ms(&mut delay),
            input: click(&mut jitter, CONFIRM_REFRESH),
        },
    ];
    Job { epoch, steps }
}

/// Buy every row in `rows`. A [`Row`] is in range by construction, so there is
/// nothing to drop here any more — a slot outside the six rows was already
/// refused by [`Slot::row`](super::Slot::row), where the caller still had a slot
/// number to put in the journal. Stateless about the list: always scroll to the
/// top first (the clamp makes it a no-op when already there), buy the top-group
/// rows, one scroll to the bottom, buy the bottom-group rows. Each buy is click +
/// confirm.
#[must_use = "a planned job that is never submitted is a lost click"]
pub fn buy_job(trigger: Trigger, timings: Timings, epoch: Epoch, rows: &[Row], seed: u64) -> Job {
    let mut jitter = Jitter::new(seed);
    let mut delay = Jitter::new(seed ^ DELAY_SEED_SALT);
    let mut rows: Vec<Row> = rows.to_vec();
    rows.sort_unstable();
    rows.dedup();
    if rows.is_empty() {
        return Job {
            epoch,
            steps: Vec::new(),
        };
    }
    let mut steps = vec![TimedStep {
        wait_ms: timings.pre_wait_ms(trigger, &mut delay),
        input: scroll(&mut jitter, SCROLL_TO_EXTREME_NOTCHES),
    }];
    // Each buy draws its own settle/between-buys wait, so the pacing varies
    // step to step; the pre-scroll before the bottom group draws afresh too.
    let mut wait_ms = timings.scroll_settle_ms(&mut delay);
    let mut at_bottom = false;
    for row in rows {
        if row.get() > LAST_TOP_ROW && !at_bottom {
            steps.push(TimedStep {
                wait_ms,
                input: scroll(&mut jitter, -SCROLL_TO_EXTREME_NOTCHES),
            });
            at_bottom = true;
            wait_ms = timings.scroll_settle_ms(&mut delay);
        }
        steps.push(TimedStep {
            wait_ms,
            input: click(&mut jitter, buy_zone(row, at_bottom)),
        });
        steps.push(TimedStep {
            wait_ms: timings.buy_modal_ms(&mut delay),
            input: click(&mut jitter, CONFIRM_BUY),
        });
        wait_ms = timings.between_buys_ms(&mut delay);
    }
    Job { epoch, steps }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actuator::plan::fixtures::{assert_within, range, row};
    use crate::actuator::plan::{MAX_TIMING_MS, Slot};

    fn click_at(step: &TimedStep) -> DesignPoint {
        match step.input {
            Input::Click { at, .. } => at,
            Input::Scroll { .. } => panic!("expected a click, got {:?}", step.input),
        }
    }

    /// The three builders once their fixed arguments are bound.
    type Build = fn(Timings, u64) -> Job;

    /// Both properties below belong to the `Jitter::new(seed ^
    /// DELAY_SEED_SALT)` line all three builders share, so they are stated once
    /// over this table instead of three times over.
    fn builders() -> [(&'static str, Build); 3] {
        [
            ("confirm_retry_job", |timings, seed| {
                confirm_retry_job(CONFIRM_BUY, timings, Epoch(5), seed)
            }),
            ("refresh_job", |timings, seed| {
                refresh_job(Trigger::Refreshed, timings, Epoch(3), seed)
            }),
            ("buy_job", |timings, seed| {
                buy_job(
                    Trigger::ShopOpened,
                    timings,
                    Epoch(9),
                    &[row(0), row(4)],
                    seed,
                )
            }),
        ]
    }

    /// Every range wide: a point range returns `min_ms` without touching the
    /// jitter, which would leave the delay seed unobservable.
    fn wide_ranges() -> Timings {
        let wide = range(0, MAX_TIMING_MS);
        Timings {
            shop_opened: wide,
            refreshed: wide,
            purchase_resumed: wide,
            recovery: wide,
            confirm_refresh_modal: wide,
            buy_modal: wide,
            between_buys: wide,
            scroll_settle: wide,
        }
    }

    fn scroll_notches(step: &TimedStep) -> i32 {
        match step.input {
            Input::Scroll { notches, .. } => notches,
            Input::Click { .. } => panic!("expected a scroll, got {:?}", step.input),
        }
    }

    #[test]
    fn confirm_retry_job_single_click_in_zone() {
        let job = confirm_retry_job(CONFIRM_BUY, Timings::default(), Epoch(5), 42);
        assert_eq!(job.epoch, Epoch(5));
        assert_eq!(job.steps.len(), 1);
        assert_eq!(job.steps[0].wait_ms, 400);
        assert_within(click_at(&job.steps[0]), CONFIRM_BUY);
    }

    #[test]
    fn refresh_job_clicks_refresh_then_confirm() {
        let job = refresh_job(Trigger::Refreshed, Timings::default(), Epoch(3), 42);
        assert_eq!(job.epoch, Epoch(3));
        assert_eq!(job.steps.len(), 2);
        assert_eq!(job.steps[0].wait_ms, 780);
        assert_within(click_at(&job.steps[0]), REFRESH);
        assert_eq!(job.steps[1].wait_ms, 270);
        assert_within(click_at(&job.steps[1]), CONFIRM_REFRESH);
    }

    #[test]
    fn buy_job_orders_top_group_then_one_scroll_then_bottom_group() {
        // Unsorted, with a duplicate. (The out-of-range row this case used to
        // carry cannot be spelled any more — see
        // `a_slot_outside_the_six_rows_never_becomes_a_click`.)
        let job = buy_job(
            Trigger::ShopOpened,
            Timings::default(),
            Epoch(9),
            &[row(5), row(0), row(4), row(0)],
            42,
        );
        assert_eq!(job.epoch, Epoch(9));
        assert_eq!(job.steps.len(), 8);
        // Scroll to the top first, whatever the current position.
        assert_eq!(job.steps[0].wait_ms, 1_180);
        assert!(scroll_notches(&job.steps[0]) > 0);
        // Row 0 at scroll-top.
        assert_eq!(job.steps[1].wait_ms, 100);
        assert_within(click_at(&job.steps[1]), buy_zone(row(0), false));
        assert_eq!(job.steps[2].wait_ms, 150);
        assert_within(click_at(&job.steps[2]), CONFIRM_BUY);
        // One scroll to the bottom between the groups.
        assert_eq!(job.steps[3].wait_ms, 600);
        assert!(scroll_notches(&job.steps[3]) < 0);
        // Rows 4 and 5 at scroll-bottom.
        assert_eq!(job.steps[4].wait_ms, 100);
        assert_within(click_at(&job.steps[4]), buy_zone(row(4), true));
        assert_eq!(job.steps[5].wait_ms, 150);
        assert_within(click_at(&job.steps[5]), CONFIRM_BUY);
        assert_eq!(job.steps[6].wait_ms, 600);
        assert_within(click_at(&job.steps[6]), buy_zone(row(5), true));
        assert_eq!(job.steps[7].wait_ms, 150);
        assert_within(click_at(&job.steps[7]), CONFIRM_BUY);
    }

    #[test]
    fn buy_job_scrolls_top_then_bottom_for_a_bottom_only_row() {
        let job = buy_job(
            Trigger::PurchaseResumed,
            Timings::default(),
            Epoch(1),
            &[row(4)],
            42,
        );
        assert_eq!(job.steps.len(), 4);
        assert_eq!(job.steps[0].wait_ms, 400);
        assert!(scroll_notches(&job.steps[0]) > 0);
        assert_eq!(job.steps[1].wait_ms, 100);
        assert!(scroll_notches(&job.steps[1]) < 0);
        assert_eq!(job.steps[2].wait_ms, 100);
        assert_within(click_at(&job.steps[2]), buy_zone(row(4), true));
    }

    #[test]
    fn a_slot_outside_the_six_rows_never_becomes_a_click() {
        // `buy_job` used to take `&[u8]` and filter the out-of-range rows out
        // itself, which meant a caller that passed *slot* numbers lost row 6
        // instead of being refused. The refusal now happens one step earlier and
        // exactly once, so there is no row left for `buy_job` to drop: slot 7 and
        // a clamped `effective_slot` fallback (`u8::MAX`) have no row at all, and
        // the resulting empty plan clicks nothing.
        assert_eq!(Slot::new(7).row(), None);
        assert_eq!(Slot::new(u8::MAX).row(), None);
        let rows: Vec<Row> = [7, u8::MAX]
            .into_iter()
            .filter_map(|slot| Slot::new(slot).row())
            .collect();
        let job = buy_job(Trigger::ShopOpened, Timings::default(), Epoch(7), &rows, 42);
        assert_eq!(job.epoch, Epoch(7));
        assert!(job.steps.is_empty());
    }

    #[test]
    fn extra_ranges_add_a_bounded_draw_on_top_of_the_baselines() {
        // Every draw lands on the step it names, within [baseline+min,
        // baseline+max], and no click position moves (the delay stream is
        // salted apart from the position stream).
        let timings = Timings {
            refreshed: range(200, 800),
            confirm_refresh_modal: range(50, 150),
            ..Timings::default()
        };
        let job = refresh_job(Trigger::Refreshed, timings, Epoch(3), 42);
        assert!((780 + 200..=780 + 800).contains(&job.steps[0].wait_ms));
        assert!((270 + 50..=270 + 150).contains(&job.steps[1].wait_ms));
        let baseline = refresh_job(Trigger::Refreshed, Timings::default(), Epoch(3), 42);
        assert_eq!(click_at(&job.steps[0]), click_at(&baseline.steps[0]));
        assert_eq!(click_at(&job.steps[1]), click_at(&baseline.steps[1]));
    }

    #[test]
    fn a_point_range_resolves_to_the_baseline_plus_that_point() {
        // min == max is a fixed extra: no randomness, an exact wait.
        let timings = Timings {
            refreshed: range(500, 500),
            ..Timings::default()
        };
        let job = refresh_job(Trigger::Refreshed, timings, Epoch(3), 42);
        assert_eq!(job.steps[0].wait_ms, 780 + 500);
    }

    #[test]
    fn draws_are_deterministic_per_seed_and_vary_across_seeds() {
        let timings = Timings {
            refreshed: range(0, 1_000),
            ..Timings::default()
        };
        let a = refresh_job(Trigger::Refreshed, timings, Epoch(3), 42);
        let b = refresh_job(Trigger::Refreshed, timings, Epoch(3), 42);
        assert_eq!(a.steps[0].wait_ms, b.steps[0].wait_ms); // same seed
        // A different seed almost surely lands on a different draw over a wide
        // range; scan a few so the test never flakes on one unlucky collision.
        let differs = (100..110).any(|seed| {
            refresh_job(Trigger::Refreshed, timings, Epoch(3), seed).steps[0].wait_ms
                != a.steps[0].wait_ms
        });
        assert!(differs, "a wide range should vary across seeds");
    }

    #[test]
    fn extra_buy_ranges_land_on_scroll_confirm_and_between_buys() {
        let timings = Timings {
            shop_opened: range(200, 200),
            scroll_settle: range(50, 50),
            buy_modal: range(30, 30),
            between_buys: range(400, 400),
            ..Timings::default()
        };
        // Point ranges keep the assertions exact while still exercising the
        // draw path on every buy step.
        let job = buy_job(
            Trigger::ShopOpened,
            timings,
            Epoch(9),
            &[row(0), row(1)],
            42,
        );
        assert_eq!(job.steps[0].wait_ms, 1_180 + 200); // pre-wait on the scroll
        assert_eq!(job.steps[1].wait_ms, 100 + 50); // scroll settle before buy 0
        assert_eq!(job.steps[2].wait_ms, 150 + 30); // confirm buy 0
        assert_eq!(job.steps[3].wait_ms, 600 + 400); // between buys, before buy 1
        assert_eq!(job.steps[4].wait_ms, 150 + 30); // confirm buy 1
    }

    #[test]
    fn confirm_retry_job_folds_in_the_recovery_range() {
        let timings = Timings {
            recovery: range(250, 250),
            ..Timings::default()
        };
        let job = confirm_retry_job(CONFIRM_REFRESH, timings, Epoch(5), 42);
        assert_eq!(job.steps[0].wait_ms, 400 + 250);
    }

    #[test]
    fn every_builder_maps_distinct_seeds_to_distinct_delay_streams() {
        // What the salt buys is a bijection: two sessions never share a click
        // rhythm. The probe seeds are picked so both ways of losing it show. The
        // salt has no bit below bit 12, so a masking salt sends *every* seed
        // under 4096 to one stream — 42 against 43 catches that. A setting salt
        // cannot see a bit it already sets, and bit 12 is one of them — 42
        // against 42 | 0x1000 catches that. Small seeds alone miss the second,
        // large ones alone miss the first.
        let seeds = [42_u64, 43, 42 | 0x1000];
        let timings = wide_ranges();
        for (name, build) in builders() {
            let probes = seeds.map(|seed| (seed, build(timings, seed).steps[0].wait_ms));
            for (i, &(seed_a, wait_a)) in probes.iter().enumerate() {
                for &(seed_b, wait_b) in &probes[i + 1..] {
                    assert_ne!(
                        wait_a, wait_b,
                        "{name}: seeds {seed_a} and {seed_b} share a delay stream"
                    );
                }
            }
        }
    }

    #[test]
    fn no_builder_lets_the_timings_move_an_input() {
        // The other half of the contract: the waits are drawn from a stream of
        // their own, so widening every range leaves positions and press holds
        // byte-identical. `extra_ranges_add_a_bounded_draw_on_top_of_the_baselines`
        // says this of `refresh_job`; the two builders that spend money had no
        // such test.
        for (name, build) in builders() {
            let salted = build(wide_ranges(), 42);
            let baseline = build(Timings::default(), 42);
            assert_eq!(salted.steps.len(), baseline.steps.len(), "{name}");
            for (step, base) in salted.steps.iter().zip(&baseline.steps) {
                assert_eq!(step.input, base.input, "{name} moved an input");
            }
        }
    }

    #[test]
    fn the_widest_legal_range_draws_inside_itself() {
        // `draw` is plain arithmetic now that `try_new` bounds the range, so the
        // widest thing it can ever see is the one worth pinning: the inclusive
        // `span + 1` modulus must not overflow (that is what `% 0` used to come
        // from) and the draw must land in the range it was asked for.
        let timings = Timings {
            refreshed: range(0, MAX_TIMING_MS),
            ..Timings::default()
        };
        let job = refresh_job(Trigger::Refreshed, timings, Epoch(3), 42);
        assert!((780..=780 + MAX_TIMING_MS).contains(&job.steps[0].wait_ms));
    }
}
