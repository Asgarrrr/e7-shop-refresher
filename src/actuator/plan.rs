//! Pure click plans: design-space zones, the design→screen transform, and
//! the timed input sequences the executor replays. No I/O — everything here
//! is computed from a client rect, an epoch, and a seed.
//!
//! Coordinates are in the game's 1280×720 design space, origin top-left.

const DESIGN_W: f32 = 1280.0;
const DESIGN_H: f32 = 720.0;

/// The game caps its content at this aspect and pillarboxes wider windows,
/// keeping the content ratio — `off_x > 0` only past the cap.
pub const MAX_ASPECT: f32 = 2.194;

// Block-animation waits (dispatch margin included): the game ignores input
// while the matching animation runs, so every step waits before it acts.
const WAIT_SHOP_OPENED_MS: u64 = 1_180;
const WAIT_REFRESHED_MS: u64 = 780;
const WAIT_PURCHASE_RESUMED_MS: u64 = 400;
/// A watchdog retry fires into an idle game (the awaited animation never
/// played): dispatch margin only.
const WAIT_RECOVERY_MS: u64 = 400;
const WAIT_CONFIRM_REFRESH_MODAL_MS: u64 = 270;
const WAIT_BUY_MODAL_MS: u64 = 150;
const WAIT_BETWEEN_BUYS_MS: u64 = 600;
/// A wheel scroll blocks nothing: only input-dispatch time before the click.
const WAIT_SCROLL_SETTLE_MS: u64 = 100;

/// Wheel notches for one scroll-to-extreme — generous on purpose: the list
/// clamps, so overshooting is free and the resulting position deterministic.
const SCROLL_TO_EXTREME_NOTCHES: i32 = 10;

/// How an element rides a non-16:9 window: HUD elements anchor to a content
/// edge, modals stay centered. In 16:9 all three coincide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    Left,
    Right,
    Center,
}

/// A clickable zone: center + size, design space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Zone {
    pub cx: f32,
    pub cy: f32,
    pub w: f32,
    pub h: f32,
    pub anchor: Anchor,
}

pub const REFRESH: Zone = Zone {
    cx: 218.5,
    cy: 664.0,
    w: 309.0,
    h: 72.0,
    anchor: Anchor::Left,
};

pub const CONFIRM_REFRESH: Zone = Zone {
    cx: 747.5,
    cy: 462.0,
    w: 200.0,
    h: 44.0,
    anchor: Anchor::Center,
};

pub const CONFIRM_BUY: Zone = Zone {
    cx: 750.0,
    cy: 508.0,
    w: 300.0,
    h: 62.0,
    anchor: Anchor::Center,
};

/// Anywhere over the item column: only the wheel routing matters.
const SCROLL_ZONE: Zone = Zone {
    cx: 1154.0,
    cy: 420.0,
    w: 190.0,
    h: 400.0,
    anchor: Anchor::Right,
};

/// The clickable 0-based row of a 1-based display slot; `None` for anything
/// a degraded shop put outside the six rows — never a click.
pub fn row_for_slot(slot: u8) -> Option<u8> {
    slot.checked_sub(1).filter(|&row| row <= 5)
}

/// The Buy button of a 0-based row. Rows 0..=3 are clickable at scroll-top;
/// 4..=5 only at scroll-bottom, where the whole list sits 217 design px
/// higher.
pub fn buy_zone(row: u8, at_bottom: bool) -> Zone {
    let mut cy = 166.5 + 145.0 * f32::from(row);
    if at_bottom {
        cy -= 217.0;
    }
    Zone {
        cx: 1154.0,
        cy,
        w: 190.0,
        h: 61.0,
        anchor: Anchor::Right,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DesignPoint {
    pub x: f32,
    pub y: f32,
    pub anchor: Anchor,
}

/// The game window's client area: physical pixels, screen origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientRect {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

/// Design → physical screen pixels, for any window at least 16:9 wide. A
/// narrower window caps the view vertically instead: unsupported, refused —
/// the caller must never guess a coordinate.
pub fn to_screen(rect: ClientRect, point: DesignPoint) -> Result<(i32, i32), String> {
    if rect.width <= 0 || rect.height <= 0 {
        return Err(format!(
            "degenerate client area {}×{}",
            rect.width, rect.height
        ));
    }
    let (cw, ch) = (rect.width as f32, rect.height as f32);
    let aspect = cw / ch;
    if aspect + 1e-3 < DESIGN_W / DESIGN_H {
        return Err(format!(
            "window aspect {aspect:.3} is narrower than 16:9 — widen the game window"
        ));
    }
    let s = ch / DESIGN_H;
    let view_w = DESIGN_H * aspect.min(MAX_ASPECT);
    let off_x = (cw - view_w * s) / 2.0;
    let x = match point.anchor {
        Anchor::Left => point.x,
        Anchor::Right => view_w - (DESIGN_W - point.x),
        Anchor::Center => view_w / 2.0 + (point.x - DESIGN_W / 2.0),
    };
    let px = (rect.left as f32 + off_x + x * s).round() as i32;
    let py = (rect.top as f32 + point.y * s).round() as i32;
    Ok((px, py))
}

/// What produced the job — decides how long the game blocks input before the
/// first act can land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    ShopOpened,
    Refreshed,
    PurchaseResumed,
    /// A watchdog re-issue: the game sits idle, no animation to wait out.
    Recovery,
}

impl Trigger {
    pub fn pre_wait_ms(self) -> u64 {
        match self {
            Trigger::ShopOpened => WAIT_SHOP_OPENED_MS,
            Trigger::Refreshed => WAIT_REFRESHED_MS,
            Trigger::PurchaseResumed => WAIT_PURCHASE_RESUMED_MS,
            Trigger::Recovery => WAIT_RECOVERY_MS,
        }
    }
}

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

/// A full input sequence, valid for one shop state: `epoch` is the snapshot
/// generation the plan was built against — the executor drops the job once a
/// newer shop has arrived.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub epoch: u64,
    pub steps: Vec<TimedStep>,
}

/// Deterministic per-seed click randomizer (xorshift64*): points land in the
/// central 75% of a zone and press holds vary, so no two clicks look alike
/// while tests stay reproducible.
pub struct Jitter(u64);

impl Jitter {
    pub fn new(seed: u64) -> Self {
        // xorshift never leaves state 0: remap to an arbitrary odd constant.
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Uniform within the central 75% of the zone: every click stays well
    /// inside the button.
    pub fn point_in(&mut self, zone: Zone) -> DesignPoint {
        DesignPoint {
            x: zone.cx + (self.unit() - 0.5) * 0.75 * zone.w,
            y: zone.cy + (self.unit() - 0.5) * 0.75 * zone.h,
            anchor: zone.anchor,
        }
    }

    /// Mouse-button hold before release.
    pub fn press_ms(&mut self) -> u64 {
        40 + self.next() % 51
    }
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
pub fn confirm_retry_job(zone: Zone, epoch: u64, seed: u64) -> Job {
    let mut jitter = Jitter::new(seed);
    Job {
        epoch,
        steps: vec![TimedStep {
            wait_ms: WAIT_RECOVERY_MS,
            input: click(&mut jitter, zone),
        }],
    }
}

/// Refresh = click Refresh, wait out the confirm modal, click its yes.
pub fn refresh_job(trigger: Trigger, epoch: u64, seed: u64) -> Job {
    let mut jitter = Jitter::new(seed);
    let steps = vec![
        TimedStep {
            wait_ms: trigger.pre_wait_ms(),
            input: click(&mut jitter, REFRESH),
        },
        TimedStep {
            wait_ms: WAIT_CONFIRM_REFRESH_MODAL_MS,
            input: click(&mut jitter, CONFIRM_REFRESH),
        },
    ];
    Job { epoch, steps }
}

/// Buy every row in `rows` (0-based; out-of-range rows are dropped, never
/// clicked). Stateless about the list: always scroll to the top first (the
/// clamp makes it a no-op when already there), buy the top-group rows, one
/// scroll to the bottom, buy the bottom-group rows. Each buy is click +
/// confirm.
pub fn buy_job(trigger: Trigger, epoch: u64, rows: &[u8], seed: u64) -> Job {
    let mut jitter = Jitter::new(seed);
    let mut rows: Vec<u8> = rows.iter().copied().filter(|&row| row <= 5).collect();
    rows.sort_unstable();
    rows.dedup();
    if rows.is_empty() {
        return Job {
            epoch,
            steps: Vec::new(),
        };
    }
    let mut steps = vec![TimedStep {
        wait_ms: trigger.pre_wait_ms(),
        input: scroll(&mut jitter, SCROLL_TO_EXTREME_NOTCHES),
    }];
    let mut wait_ms = WAIT_SCROLL_SETTLE_MS;
    let mut at_bottom = false;
    for row in rows {
        if row > 3 && !at_bottom {
            steps.push(TimedStep {
                wait_ms,
                input: scroll(&mut jitter, -SCROLL_TO_EXTREME_NOTCHES),
            });
            at_bottom = true;
            wait_ms = WAIT_SCROLL_SETTLE_MS;
        }
        steps.push(TimedStep {
            wait_ms,
            input: click(&mut jitter, buy_zone(row, at_bottom)),
        });
        steps.push(TimedStep {
            wait_ms: WAIT_BUY_MODAL_MS,
            input: click(&mut jitter, CONFIRM_BUY),
        });
        wait_ms = WAIT_BETWEEN_BUYS_MS;
    }
    Job { epoch, steps }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, width: i32, height: i32) -> ClientRect {
        ClientRect {
            left,
            top,
            width,
            height,
        }
    }

    fn point(x: f32, y: f32, anchor: Anchor) -> DesignPoint {
        DesignPoint { x, y, anchor }
    }

    fn click_at(step: &TimedStep) -> DesignPoint {
        match step.input {
            Input::Click { at, .. } => at,
            Input::Scroll { .. } => panic!("expected a click, got {:?}", step.input),
        }
    }

    fn scroll_notches(step: &TimedStep) -> i32 {
        match step.input {
            Input::Scroll { notches, .. } => notches,
            Input::Click { .. } => panic!("expected a scroll, got {:?}", step.input),
        }
    }

    /// Within the central 75% of the zone, correct anchor.
    fn assert_within(at: DesignPoint, zone: Zone) {
        assert_eq!(at.anchor, zone.anchor);
        assert!(
            (at.x - zone.cx).abs() <= 0.375 * zone.w,
            "x {} escapes zone {zone:?}",
            at.x
        );
        assert!(
            (at.y - zone.cy).abs() <= 0.375 * zone.h,
            "y {} escapes zone {zone:?}",
            at.y
        );
    }

    #[test]
    fn to_screen_is_identity_at_design_resolution() {
        let rect = rect(0, 0, 1280, 720);
        assert_eq!(
            to_screen(rect, point(100.0, 100.0, Anchor::Left)),
            Ok((100, 100))
        );
        assert_eq!(
            to_screen(rect, point(640.0, 462.0, Anchor::Center)),
            Ok((640, 462))
        );
        assert_eq!(
            to_screen(rect, point(1154.0, 664.0, Anchor::Right)),
            Ok((1154, 664))
        );
    }

    #[test]
    fn to_screen_scales_and_offsets_at_16_9() {
        // 1920×1080 at (100, 50): s = 1.5, no pillarbox.
        let rect = rect(100, 50, 1920, 1080);
        assert_eq!(
            to_screen(rect, point(100.0, 100.0, Anchor::Left)),
            Ok((250, 200))
        );
        assert_eq!(
            to_screen(rect, point(640.0, 100.0, Anchor::Center)),
            Ok((1060, 200))
        );
        assert_eq!(
            to_screen(rect, point(1154.0, 100.0, Anchor::Right)),
            Ok((1831, 200))
        );
    }

    #[test]
    fn to_screen_anchors_follow_the_view_edges_when_wide() {
        // 1440×720 (aspect 2.0, under the cap): the view fills the window, so
        // Left sticks to the left edge, Right to the right edge, Center stays
        // centered.
        let rect = rect(0, 0, 1440, 720);
        assert_eq!(
            to_screen(rect, point(100.0, 100.0, Anchor::Left)),
            Ok((100, 100))
        );
        assert_eq!(
            to_screen(rect, point(640.0, 100.0, Anchor::Center)),
            Ok((720, 100))
        );
        // 1154 sits 126 design px from the right design edge.
        assert_eq!(
            to_screen(rect, point(1154.0, 100.0, Anchor::Right)),
            Ok((1440 - 126, 100))
        );
    }

    #[test]
    fn to_screen_pillarboxes_past_the_aspect_cap() {
        // 3440×1440 (aspect 2.389 > cap): symmetric side bars appear and the
        // anchors follow the content edges, not the screen edges.
        let rect = rect(0, 0, 3440, 1440);
        let (left_edge, _) = to_screen(rect, point(0.0, 0.0, Anchor::Left)).unwrap();
        let (right_edge, _) = to_screen(rect, point(1280.0, 0.0, Anchor::Right)).unwrap();
        assert_eq!(left_edge, 140);
        assert_eq!(right_edge, 3300);
        assert_eq!(left_edge, 3440 - right_edge); // symmetric bars
    }

    #[test]
    fn to_screen_refuses_a_narrow_window() {
        let narrow = rect(0, 0, 1280, 800);
        assert!(to_screen(narrow, point(100.0, 100.0, Anchor::Left)).is_err());
    }

    #[test]
    fn to_screen_refuses_a_degenerate_rect() {
        assert!(to_screen(rect(0, 0, 0, 0), point(0.0, 0.0, Anchor::Left)).is_err());
    }

    #[test]
    fn jitter_is_deterministic_per_seed() {
        let mut a = Jitter::new(7);
        let mut b = Jitter::new(7);
        for _ in 0..5 {
            assert_eq!(a.point_in(REFRESH), b.point_in(REFRESH));
            assert_eq!(a.press_ms(), b.press_ms());
        }
    }

    #[test]
    fn jitter_seeds_diverge() {
        let mut c = Jitter::new(8);
        assert_ne!(Jitter::new(7).point_in(REFRESH), c.point_in(REFRESH));
    }

    #[test]
    fn jitter_stays_in_the_central_band_and_hold_range() {
        let mut jitter = Jitter::new(1234);
        for _ in 0..200 {
            assert_within(jitter.point_in(CONFIRM_BUY), CONFIRM_BUY);
            let hold = jitter.press_ms();
            assert!((40..=90).contains(&hold), "hold {hold} out of range");
        }
    }

    #[test]
    fn jitter_seed_zero_is_not_degenerate() {
        let mut jitter = Jitter::new(0);
        let first = jitter.point_in(REFRESH);
        let second = jitter.point_in(REFRESH);
        assert_ne!(first, second);
    }

    #[test]
    fn row_for_slot_maps_the_six_slots_and_rejects_the_rest() {
        assert_eq!(row_for_slot(1), Some(0));
        assert_eq!(row_for_slot(6), Some(5));
        assert_eq!(row_for_slot(0), None);
        assert_eq!(row_for_slot(7), None);
    }

    #[test]
    fn buy_zone_positions_top_and_bottom_rows() {
        assert_eq!(
            buy_zone(0, false),
            Zone {
                cx: 1154.0,
                cy: 166.5,
                w: 190.0,
                h: 61.0,
                anchor: Anchor::Right,
            }
        );
        assert_eq!(buy_zone(4, true).cy, 529.5);
        assert_eq!(buy_zone(5, true).cy, 674.5);
    }

    #[test]
    fn trigger_pre_waits_cover_each_animation() {
        assert_eq!(Trigger::ShopOpened.pre_wait_ms(), 1_180);
        assert_eq!(Trigger::Refreshed.pre_wait_ms(), 780);
        assert_eq!(Trigger::PurchaseResumed.pre_wait_ms(), 400);
        // Recovery fires into an idle game: dispatch margin only.
        assert_eq!(Trigger::Recovery.pre_wait_ms(), 400);
    }

    #[test]
    fn confirm_retry_job_single_click_in_zone() {
        let job = confirm_retry_job(CONFIRM_BUY, 5, 42);
        assert_eq!(job.epoch, 5);
        assert_eq!(job.steps.len(), 1);
        assert_eq!(job.steps[0].wait_ms, 400);
        assert_within(click_at(&job.steps[0]), CONFIRM_BUY);
    }

    #[test]
    fn refresh_job_clicks_refresh_then_confirm() {
        let job = refresh_job(Trigger::Refreshed, 3, 42);
        assert_eq!(job.epoch, 3);
        assert_eq!(job.steps.len(), 2);
        assert_eq!(job.steps[0].wait_ms, 780);
        assert_within(click_at(&job.steps[0]), REFRESH);
        assert_eq!(job.steps[1].wait_ms, 270);
        assert_within(click_at(&job.steps[1]), CONFIRM_REFRESH);
    }

    #[test]
    fn buy_job_orders_top_group_then_one_scroll_then_bottom_group() {
        // Unsorted with a duplicate and an out-of-range row.
        let job = buy_job(Trigger::ShopOpened, 9, &[5, 0, 4, 0, 6], 42);
        assert_eq!(job.epoch, 9);
        assert_eq!(job.steps.len(), 8);
        // Scroll to the top first, whatever the current position.
        assert_eq!(job.steps[0].wait_ms, 1_180);
        assert!(scroll_notches(&job.steps[0]) > 0);
        // Row 0 at scroll-top.
        assert_eq!(job.steps[1].wait_ms, 100);
        assert_within(click_at(&job.steps[1]), buy_zone(0, false));
        assert_eq!(job.steps[2].wait_ms, 150);
        assert_within(click_at(&job.steps[2]), CONFIRM_BUY);
        // One scroll to the bottom between the groups.
        assert_eq!(job.steps[3].wait_ms, 600);
        assert!(scroll_notches(&job.steps[3]) < 0);
        // Rows 4 and 5 at scroll-bottom.
        assert_eq!(job.steps[4].wait_ms, 100);
        assert_within(click_at(&job.steps[4]), buy_zone(4, true));
        assert_eq!(job.steps[5].wait_ms, 150);
        assert_within(click_at(&job.steps[5]), CONFIRM_BUY);
        assert_eq!(job.steps[6].wait_ms, 600);
        assert_within(click_at(&job.steps[6]), buy_zone(5, true));
        assert_eq!(job.steps[7].wait_ms, 150);
        assert_within(click_at(&job.steps[7]), CONFIRM_BUY);
    }

    #[test]
    fn buy_job_scrolls_top_then_bottom_for_a_bottom_only_row() {
        let job = buy_job(Trigger::PurchaseResumed, 1, &[4], 42);
        assert_eq!(job.steps.len(), 4);
        assert_eq!(job.steps[0].wait_ms, 400);
        assert!(scroll_notches(&job.steps[0]) > 0);
        assert_eq!(job.steps[1].wait_ms, 100);
        assert!(scroll_notches(&job.steps[1]) < 0);
        assert_eq!(job.steps[2].wait_ms, 100);
        assert_within(click_at(&job.steps[2]), buy_zone(4, true));
    }

    #[test]
    fn buy_job_drops_out_of_range_rows_entirely() {
        // A clamped fallback slot must never become a click.
        let job = buy_job(Trigger::ShopOpened, 7, &[6, u8::MAX], 42);
        assert_eq!(job.epoch, 7);
        assert!(job.steps.is_empty());
    }
}
