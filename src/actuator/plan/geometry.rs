//! Where a click goes: the 1280×720 design space, the clickable zones, the two
//! row/slot representations, and the transform into physical screen pixels.
//!
//! The bottom layer of `plan` — it answers *where*, never *when*, and depends on
//! nothing else in the module. `MAX_ROW`, `LAST_TOP_ROW`, [`buy_zone`] and the
//! test that couples them live here together on purpose: "six rows, the first
//! four reachable at scroll-top" is a statement about *this* geometry, and the
//! guard test reads the row count off `buy_zone` and `DESIGN_H` to prove it.

const DESIGN_W: f32 = 1280.0;
const DESIGN_H: f32 = 720.0;

/// The game caps its content at this aspect and pillarboxes wider windows,
/// keeping the content ratio — `off_x > 0` only past the cap.
pub const MAX_ASPECT: f32 = 2.194;

/// Highest 0-based clickable row: the Secret Shop shows six display slots.
const MAX_ROW: u8 = 5;
/// Highest row reachable at scroll-top; anything above it is only clickable
/// once the list has been scrolled to the bottom.
///
/// `pub(super)` for [`buy_job`](super::jobs::buy_job), the one reader outside
/// this file — the split of the rows into two groups is its whole scrolling
/// strategy.
pub(super) const LAST_TOP_ROW: u8 = 3;
/// `MAX_ROW` and `LAST_TOP_ROW` are two halves of one fact ("six rows, the
/// first four reachable at scroll-top"). Do not let them drift back into bare
/// `<= 5` / `> 3` literals at separate sites: a shop row count changed in one
/// place only would plan a scroll-to-bottom for a row still at the top, i.e. a
/// click on the wrong item with real gold behind it. Editing either alone now
/// stops the build here.
const _: () = assert!(LAST_TOP_ROW < MAX_ROW);

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
///
/// `pub(super)` for `jobs::scroll`, which is the only thing that ever aims at
/// it; it stays here with the other three zones because the four together are
/// the design-space map.
pub(super) const SCROLL_ZONE: Zone = Zone {
    cx: 1154.0,
    cy: 420.0,
    w: 190.0,
    h: 400.0,
    anchor: Anchor::Right,
};

/// A 1-based display slot, exactly as the shop numbers its items — the shape
/// the domain's `BuyTarget::slot` and `ShopItem::effective_slot` speak.
///
/// Distinct from [`Row`] because the two differ by one and used to be the same
/// `u8`: `buy_job(trigger, timings, epoch, &slots, now_ms)` type-checked, and an
/// off-by-one row clicks the *wrong item's* Buy button, spends the player's gold
/// on an item the filter rejected, and then wedges the pause — the purchase echo
/// for the unexpected id is not on the checklist, so nothing ever clears it.
/// [`Slot::row`] is the one place the two representations meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Slot(u8);

impl Slot {
    /// Wraps a display slot number as the shop reported it. Any value is
    /// accepted — a degraded shop can report 0 or a clamped `u8::MAX`, and
    /// [`Slot::row`] is what refuses those.
    #[must_use]
    pub const fn new(slot: u8) -> Self {
        Self(slot)
    }

    /// The display number, for the player-facing line.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// The clickable 0-based row of this display slot; `None` for anything
    /// a degraded shop put outside the six rows — never a click.
    ///
    /// # Examples
    ///
    /// ```
    /// use arkyve_refresh_shop::actuator::plan::{Row, Slot};
    ///
    /// // The six display slots the shop numbers 1..=6 are rows 0..=5.
    /// assert_eq!(Slot::new(1).row(), Row::new(0));
    /// assert_eq!(Slot::new(6).row(), Row::new(5));
    ///
    /// // What a degraded shop can report: the `0` sentinel, a seventh slot,
    /// // and the clamped `effective_slot` fallback. None of them is a click.
    /// assert_eq!(Slot::new(0).row(), None);
    /// assert_eq!(Slot::new(7).row(), None);
    /// assert_eq!(Slot::new(u8::MAX).row(), None);
    /// ```
    #[must_use]
    pub const fn row(self) -> Option<Row> {
        match self.0.checked_sub(1) {
            Some(row) if row <= MAX_ROW => Some(Row(row)),
            _ => None,
        }
    }
}

/// A 0-based clickable row, `0..=MAX_ROW` **by construction**: the only thing
/// [`buy_zone`] and [`buy_job`](super::jobs::buy_job) accept.
///
/// `buy_job` used to filter `row <= MAX_ROW` itself and silently lose anything
/// above it, so a slot-numbered `&[1, …, 6]` dropped row 6 rather than erring.
/// There is nothing left to filter: an out-of-range row cannot be built, and the
/// refusal happens once, at [`Slot::row`], where the caller still has a slot to
/// name in the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Row(u8);

impl Row {
    /// A row from a raw 0-based index, or `None` past [`MAX_ROW`].
    #[must_use]
    pub const fn new(row: u8) -> Option<Self> {
        if row <= MAX_ROW {
            Some(Self(row))
        } else {
            None
        }
    }

    /// The 0-based index, for the coordinate arithmetic.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }

    /// The display slot this row is: the one definition of the `row + 1` the
    /// journal line used to spell by hand. Cannot overflow — a `Row` is at most
    /// `MAX_ROW`.
    #[must_use]
    pub const fn slot(self) -> Slot {
        Slot(self.0 + 1)
    }
}

/// Row 0's Buy-button centre at scroll-top, design px.
const BUY_ROW_TOP_CY: f32 = 166.5;
/// Design px between two consecutive Buy buttons.
const BUY_ROW_PITCH: f32 = 145.0;
/// How far the whole list rides up once scrolled to the bottom, design px.
const SCROLL_BOTTOM_SHIFT: f32 = 217.0;

/// The Buy button of a 0-based row. Rows `0..=LAST_TOP_ROW` are clickable at
/// scroll-top; the rest only at scroll-bottom, where the whole list sits
/// `SCROLL_BOTTOM_SHIFT` design px higher.
///
/// # Examples
///
/// ```
/// use arkyve_refresh_shop::actuator::plan::{Row, buy_zone};
/// # fn row(index: u8) -> Row { Row::new(index).expect("0..=5 is a real row") }
///
/// // Row 0's Buy button is on the 720 px-tall screen at scroll-top…
/// assert_eq!(buy_zone(row(0), false).cy, 166.5);
/// // …while row 4's is not: it only exists once the list has been scrolled
/// // to the bottom, which lifts the whole list by the same 217 px.
/// assert_eq!(buy_zone(row(4), false).cy, 746.5);
/// assert_eq!(buy_zone(row(4), true).cy, 529.5);
/// ```
pub fn buy_zone(row: Row, at_bottom: bool) -> Zone {
    let mut cy = BUY_ROW_TOP_CY + BUY_ROW_PITCH * f32::from(row.get());
    if at_bottom {
        cy -= SCROLL_BOTTOM_SHIFT;
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

impl ClientRect {
    /// No usable client area at all — which on Windows is what a *minimized*
    /// window reads back as, so it is the recoverable case everywhere: the next
    /// `acquire()` re-reads a fresh rect and the watchdog's retry self-heals it.
    ///
    /// The single definition matters: do not re-spell this test at a call
    /// site — every caller (the executor's post-`acquire` guard, each Win32
    /// backend) must agree on it, or a transient minimize can halt the watch.
    #[must_use]
    pub const fn is_degenerate(self) -> bool {
        self.width <= 0 || self.height <= 0
    }
}

/// Why a design point has no screen coordinate.
///
/// Two variants rather than one string because the two want *opposite* verdicts
/// from the executor — a minimized window aborts one job, an unsupported window
/// shape halts the watch — and a caller that cannot match on the cause has to
/// re-derive one of them itself.
/// `PartialEq` but not `Eq`: `TooNarrow` carries the measured aspect, and the
/// point of keeping it is the message, not equality.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScreenError {
    /// The client area has no extent: minimized, or a window that just died.
    /// Recoverable — the next `acquire()` reads a fresh rect.
    DegenerateRect { width: i32, height: i32 },
    /// Narrower than 16:9, so the game caps the view *vertically* and no
    /// design-space mapping exists. Only the player can fix it.
    TooNarrow { aspect: f32 },
}

impl std::fmt::Display for ScreenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            ScreenError::DegenerateRect { width, height } => {
                write!(f, "degenerate client area {width}×{height}")
            }
            ScreenError::TooNarrow { aspect } => write!(
                f,
                "window aspect {aspect:.3} is narrower than 16:9 — widen the game window"
            ),
        }
    }
}

/// Design → physical screen pixels, for any window at least 16:9 wide. A
/// narrower window caps the view vertically instead: unsupported, refused —
/// the caller must never guess a coordinate.
///
/// # Errors
///
/// [`ScreenError::DegenerateRect`] when `rect` has no extent (a minimized or
/// just-closed window), and [`ScreenError::TooNarrow`] when the window is
/// narrower than the 16:9 design aspect. The distinction is the whole point of
/// the type: the first is transient, the second is not.
///
/// # Examples
///
/// ```
/// use arkyve_refresh_shop::actuator::plan::{
///     Anchor, ClientRect, DesignPoint, ScreenError, to_screen,
/// };
///
/// let left = DesignPoint { x: 100.0, y: 100.0, anchor: Anchor::Left };
///
/// // At the 1280×720 design resolution the transform is the identity, and the
/// // three anchors coincide.
/// let design = ClientRect { left: 0, top: 0, width: 1280, height: 720 };
/// assert_eq!(to_screen(design, left)?, (100, 100));
///
/// // 1920×1080 at screen (100, 50): scale 1.5, no pillarbox.
/// let scaled = ClientRect { left: 100, top: 50, width: 1920, height: 1080 };
/// assert_eq!(to_screen(scaled, left)?, (250, 200));
///
/// // Narrower than 16:9 has no design-space mapping at all: the game caps the
/// // view vertically instead, so the point is refused rather than guessed.
/// let narrow = ClientRect { left: 0, top: 0, width: 1280, height: 800 };
/// assert!(matches!(
///     to_screen(narrow, left),
///     Err(ScreenError::TooNarrow { .. })
/// ));
/// # Ok::<(), ScreenError>(())
/// ```
pub fn to_screen(rect: ClientRect, point: DesignPoint) -> Result<(i32, i32), ScreenError> {
    Ok(Viewport::of(rect)?.place(point))
}

/// A window that has been proved mappable, and the three numbers that proof
/// produced. Every term in [`to_screen`] that depends on the *window* rather
/// than on the point resolves here, once.
///
/// The split exists for the executor, not for arithmetic. Both [`ScreenError`]s
/// are properties of the rect alone, so asking them per step asked the same
/// question of the same unchanging value up to eight times per job — and the
/// only place the executor could ask was inside its step loop, *after* the first
/// `sleep(step.wait_ms)`. A minimized window therefore paid a full step delay,
/// up to 61 s with a configured range, before abandoning a job that could never
/// have landed a click. Resolved once at `acquire` time, the refusal is
/// immediate and the per-step map cannot fail at all.
///
/// This does not weaken the mid-job case the executor used to be credited with
/// catching here: the rect is measured once by `acquire` and never re-read by
/// the loop, so a window minimized *during* a job was never visible to this
/// conversion. That is the backends' `validate_target`, which re-reads the rect
/// before every input and answers `Recoverable`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    /// Client-area origin in screen pixels, straight off the rect.
    left: i32,
    top: i32,
    /// Design pixels → physical pixels.
    scale: f32,
    /// Width of the 16:9-or-wider view in design pixels, which is what the
    /// `Right` and `Center` anchors measure against.
    view_w: f32,
    /// Pillarbox: half the client width the view does not cover.
    off_x: f32,
}

impl Viewport {
    /// Resolves the window-dependent half of the design → screen transform.
    ///
    /// # Errors
    ///
    /// The same two verdicts, for the same reasons, as [`to_screen`] — which is
    /// now a thin wrapper over this and [`place`](Viewport::place).
    pub fn of(rect: ClientRect) -> Result<Self, ScreenError> {
        if rect.is_degenerate() {
            return Err(ScreenError::DegenerateRect {
                width: rect.width,
                height: rect.height,
            });
        }
        let (cw, ch) = (rect.width as f32, rect.height as f32);
        let aspect = cw / ch;
        if aspect + 1e-3 < DESIGN_W / DESIGN_H {
            return Err(ScreenError::TooNarrow { aspect });
        }
        let scale = ch / DESIGN_H;
        let view_w = DESIGN_H * aspect.min(MAX_ASPECT);
        Ok(Self {
            left: rect.left,
            top: rect.top,
            scale,
            view_w,
            off_x: (cw - view_w * scale) / 2.0,
        })
    }

    /// One design point in the resolved window. Infallible by construction:
    /// the only two ways this transform has no answer are refused by
    /// [`of`](Viewport::of), and nothing between the two calls can reintroduce
    /// them — `self` carries no borrow of the world.
    #[must_use]
    pub fn place(self, point: DesignPoint) -> (i32, i32) {
        let Self {
            left,
            top,
            scale: s,
            view_w,
            off_x,
        } = self;
        let x = match point.anchor {
            Anchor::Left => point.x,
            Anchor::Right => view_w - (DESIGN_W - point.x),
            Anchor::Center => view_w / 2.0 + (point.x - DESIGN_W / 2.0),
        };
        // `as` from float to int saturates at the bounds but maps `NaN` to 0 —
        // which here would be a click at the top-left corner of the *screen*,
        // outside the game window, silently. It cannot happen: `of`'s
        // `is_degenerate` rejects `height <= 0`, so `ch >= 1.0` and neither `s`
        // nor `aspect` can be a `0.0 / 0.0`, and every other term is a finite
        // design constant or a rect field. That argument is why the refusal has
        // to stay in the constructor: it is what makes this function total. The
        // saturation itself is the wanted behaviour for the remaining extreme —
        // an absurd rect clamps to `i32` bounds, and `pack_point` then refuses
        // the coordinate rather than masking it back inside the window. The same
        // reasoning is written out at `win::move_cursor`'s clamp.
        let px = (left as f32 + off_x + x * s).round() as i32;
        let py = (top as f32 + point.y * s).round() as i32;
        (px, py)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The guard test below plans a real job: the coupling it pins runs from the
    // row count through `buy_zone` into `buy_job`, so it has to reach the
    // sibling module that builds one.
    use crate::actuator::plan::fixtures::row;
    use crate::actuator::plan::{Epoch, Input, Timings, Trigger, buy_job};

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

    /// The `Center` arm's offset term, on a point that is not the centre.
    ///
    /// Every other `Anchor::Center` assertion in this module passes `x = 640.0`,
    /// which is exactly `DESIGN_W / 2.0` — so `(point.x - DESIGN_W / 2.0)` is
    /// `0.0` at all of them and the whole offset term is unobservable. Mutation
    /// proved it: flipping the `+` to `-` and to `/` in
    /// `view_w / 2.0 + (point.x - DESIGN_W / 2.0)` leaves the entire suite green
    /// on the shipped feature lane. Neither zone that actually uses this anchor
    /// sits at the centre — `CONFIRM_REFRESH` is at 747.5 and `CONFIRM_BUY` at
    /// 750.0 — so under the sign flip the buy confirmation is aimed 220 design
    /// pixels left of its button, on a modal the executor clicks with the
    /// player's gold already committed.
    ///
    /// This is `const-003`'s failure mode with a different constant: a test that
    /// covers the line, asserts a true thing, and cannot fail. Both real zone
    /// abscissae are used below so the pin is on the values that ship, and a
    /// mirrored point on the other side of the centre catches an `abs` as well
    /// as a sign.
    #[test]
    fn to_screen_places_an_off_centre_center_anchor() {
        // Design resolution: the transform is the identity, so the expected
        // value is the design abscissa itself and any change to the offset
        // term's sign, operator or operand order moves it.
        let design = rect(0, 0, 1280, 720);
        // Genuinely pillarboxed, so the view centre and the window centre are
        // different points and the offset must ride the former. 1800×720 is
        // aspect 2.5, past the 2.194 cap, so `view_w` is 1579.68 with a 110.16
        // bar each side: 1579.68 / 2 + (750 - 640) + 110.16 = 1010.
        let wide = rect(0, 0, 1800, 720);
        assert_eq!(
            to_screen(
                design,
                point(CONFIRM_BUY.cx, CONFIRM_BUY.cy, Anchor::Center)
            ),
            Ok((750, 508))
        );
        assert_eq!(
            to_screen(
                design,
                point(CONFIRM_REFRESH.cx, CONFIRM_REFRESH.cy, Anchor::Center)
            ),
            Ok((748, 462))
        );
        // Mirrored about the centre: 640 - 110 must land 110 left of it, not
        // right of it, which an `abs` or a swapped subtraction would get wrong
        // while still passing the two above.
        assert_eq!(
            to_screen(design, point(530.0, 360.0, Anchor::Center)),
            Ok((530, 360))
        );
        assert_eq!(
            to_screen(wide, point(CONFIRM_BUY.cx, CONFIRM_BUY.cy, Anchor::Center)),
            Ok((1010, 508))
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
        let Err(error @ ScreenError::TooNarrow { .. }) =
            to_screen(narrow, point(100.0, 100.0, Anchor::Left))
        else {
            panic!("a sub-16:9 window has no design mapping");
        };
        // The wording the executor puts in front of the player, unchanged.
        assert!(error.to_string().contains("narrower than 16:9"), "{error}");
    }

    #[test]
    fn to_screen_refuses_a_degenerate_rect() {
        let Err(error) = to_screen(rect(0, 0, 0, 0), point(0.0, 0.0, Anchor::Left)) else {
            panic!("a minimized window has no client area to map into");
        };
        // Told apart from `TooNarrow` by the *type*, not by its text: this one
        // aborts a job, the other one halts the watch.
        assert_eq!(
            error,
            ScreenError::DegenerateRect {
                width: 0,
                height: 0
            }
        );
        assert_eq!(error.to_string(), "degenerate client area 0×0");
    }

    /// The three properties `to_screen` is *defined* by, over a deliberate
    /// lattice of window shapes rather than five hand-picked resolutions.
    ///
    /// Do not replace this with `proptest` (asked for by `20-test.md`'s
    /// `test-007`): `to_screen` is piecewise linear, so all its interesting
    /// behaviour is at the boundaries — exactly 16:9, exactly the aspect cap,
    /// the anchor extremes, the design-space edges — and a lattice hits every
    /// one of those deliberately where random rects would hit them by luck, for
    /// the cost of test-only crates on every `--locked` lane. A generator would
    /// also have to construct at-least-16:9 rects with the same arithmetic the
    /// function under test uses. Coverage here is 1 152 cases against the five
    /// resolutions above, and the properties are stated rather than sampled.
    #[test]
    fn to_screen_maps_every_shape_inside_the_client_area() {
        // Heights across the range a real window takes, plus the extremes; extra
        // width walks the three regimes — exactly 16:9, wider, and past the
        // `MAX_ASPECT` cap where the view pillarboxes.
        let heights = [1, 200, 719, 720, 721, 1080, 1440, 2160];
        let extras = [0, 1, 7, 400, 1920, 8000];
        let points = [
            point(0.0, 0.0, Anchor::Left),
            point(1280.0, 720.0, Anchor::Right),
            point(640.0, 360.0, Anchor::Center),
            point(1154.0, 664.0, Anchor::Right),
        ];
        let mut cases = 0_u32;
        for height in heights {
            // The narrowest width that is still at least 16:9, so every rect in
            // the sweep is one `to_screen` must accept.
            let min_width = (f64::from(height) * f64::from(DESIGN_W) / f64::from(DESIGN_H)).ceil();
            for extra in extras {
                let width = min_width as i32 + extra;
                for left in [-4000, -1, 0, 1, 2560] {
                    let r = rect(left, -3000, width, height);
                    for p in points {
                        let (px, py) = to_screen(r, p).unwrap_or_else(|err| {
                            panic!("{width}×{height} is 16:9 or wider: {err}")
                        });
                        // 1. Inside the client area. This is the property the
                        //    executor relies on and no example test stated: a
                        //    coordinate outside the window clicks another
                        //    application, or nothing, with real gold behind it.
                        assert!(
                            (r.left..=r.left + r.width).contains(&px),
                            "x {px} outside {}..={} for {width}×{height}",
                            r.left,
                            r.left + r.width
                        );
                        assert!(
                            (r.top..=r.top + r.height).contains(&py),
                            "y {py} outside {}..={} for {width}×{height}",
                            r.top,
                            r.top + r.height
                        );
                        cases += 1;
                    }
                    // 2. The pillarbox bars are symmetric: the design-space left
                    //    and right edges sit the same distance from their window
                    //    edges. An asymmetric offset is how a centred modal's
                    //    confirm button drifts off it on an ultrawide.
                    let (left_edge, _) = to_screen(r, point(0.0, 0.0, Anchor::Left)).expect("edge");
                    let (right_edge, _) =
                        to_screen(r, point(DESIGN_W, 0.0, Anchor::Right)).expect("edge");
                    let (bar_left, bar_right) = (left_edge - r.left, r.left + r.width - right_edge);
                    assert!(
                        (bar_left - bar_right).abs() <= 1,
                        "bars {bar_left}/{bar_right} differ by more than rounding at {width}×{height}"
                    );
                    // 3. Monotone in the design x within one anchor: a larger
                    //    design x never maps to a smaller pixel. `Anchor::Left`
                    //    alone, because the three anchors measure from different
                    //    edges and are not comparable to each other.
                    let mut last = i32::MIN;
                    for x in [0.0, 1.0, 320.0, 640.0, 1279.0, DESIGN_W] {
                        let (px, _) = to_screen(r, point(x, 0.0, Anchor::Left)).expect("in range");
                        assert!(px >= last, "x is not monotone at {width}×{height}");
                        last = px;
                    }
                }
            }
        }
        // The sweep is worth what it covers; a refactor that silently shrinks it
        // should fail here rather than pass quietly.
        assert_eq!(cases, 8 * 6 * 5 * 4);
    }

    #[test]
    fn a_degenerate_rect_is_recognised_by_either_missing_dimension() {
        assert!(!rect(0, 0, 1280, 720).is_degenerate());
        assert!(rect(0, 0, 0, 720).is_degenerate());
        assert!(rect(0, 0, 1280, 0).is_degenerate());
        assert!(rect(0, 0, -1, 720).is_degenerate());
    }

    #[test]
    fn the_row_count_and_the_scroll_split_stay_one_fact() {
        // The guard that was missing: `MAX_ROW` and `LAST_TOP_ROW` used to be
        // bare `<= 5` / `> 3` literals at three sites, and editing one alone
        // planned a scroll-to-bottom for a row still at the top — a click on the
        // wrong item's Buy button with real gold behind it.
        //
        // Every clause below is a *literal*, deliberately. The first version of
        // this test derived all six of its assertions from the two constants,
        // which made every one of them a tautology: `Slot::new(MAX_ROW + 1).row()
        // == Row::new(MAX_ROW)` holds for any `MAX_ROW < 255` by `Slot::row`'s own
        // definition, and the two `buy_job` clauses restated `row.get() >
        // LAST_TOP_ROW`. `LAST_TOP_ROW = 2` or `= 4` passed the whole suite —
        // i.e. the guard reintroduced exactly the hazard it was filed against.
        assert_eq!(MAX_ROW, 5, "the Secret Shop shows six display slots");
        assert_eq!(LAST_TOP_ROW, 3, "four of them are reachable at scroll-top");
        assert_eq!(Slot::new(6).row(), Row::new(5));
        assert_eq!(Slot::new(7).row(), None);
        assert_eq!(Row::new(6), None);

        // The clause that actually couples the row count to the geometry, which
        // is the coupling the two constants exist to state: a top-group row is
        // one whose Buy button is on screen *without* scrolling, and a
        // bottom-group row is one whose Buy button is on screen only *with* it.
        // Read straight off `buy_zone`, so a row count changed without moving the
        // split — or a pitch or shift changed without moving either — fails here
        // and not in the shop.
        let on_screen = |cy: f32| (0.0..=DESIGN_H).contains(&cy);
        for index in 0..=LAST_TOP_ROW {
            assert!(
                on_screen(buy_zone(row(index), false).cy),
                "row {index} is in the top group but its Buy button is off screen at scroll-top"
            );
        }
        for index in LAST_TOP_ROW + 1..=MAX_ROW {
            assert!(
                on_screen(buy_zone(row(index), true).cy),
                "row {index} is in the bottom group but its Buy button is off screen at scroll-bottom"
            );
            assert!(
                !on_screen(buy_zone(row(index), false).cy),
                "row {index} is reachable at scroll-top, so it does not belong to the bottom group"
            );
        }

        // The last top-group row is bought without a second scroll; the first
        // bottom-group row is reached by one, and only one.
        let scrolls = |index: u8| {
            buy_job(
                Trigger::ShopOpened,
                Timings::default(),
                Epoch(0),
                &[row(index)],
                42,
            )
            .steps
            .iter()
            .filter(|step| matches!(step.input, Input::Scroll { .. }))
            .count()
        };
        assert_eq!(scrolls(3), 1);
        assert_eq!(scrolls(4), 2);
        // And the row the extra scroll exists for really does move by the shift.
        assert_eq!(
            buy_zone(row(4), false).cy - buy_zone(row(4), true).cy,
            SCROLL_BOTTOM_SHIFT
        );
    }

    #[test]
    fn slot_row_maps_the_six_slots_and_rejects_the_rest() {
        assert_eq!(Slot::new(1).row(), Row::new(0));
        assert_eq!(Slot::new(6).row(), Row::new(5));
        assert_eq!(Slot::new(0).row(), None);
        assert_eq!(Slot::new(7).row(), None);
    }

    /// The two representations round-trip, in the one place each direction
    /// lives: the journal line reads `Row::slot`, the planner reads `Slot::row`.
    #[test]
    fn every_row_names_the_slot_it_came_from() {
        for index in 0..=MAX_ROW {
            let row = row(index);
            assert_eq!(row.slot().row(), Some(row));
            assert_eq!(row.slot().get(), index + 1);
        }
    }

    #[test]
    fn buy_zone_positions_top_and_bottom_rows() {
        assert_eq!(
            buy_zone(row(0), false),
            Zone {
                cx: 1154.0,
                cy: 166.5,
                w: 190.0,
                h: 61.0,
                anchor: Anchor::Right,
            }
        );
        assert_eq!(buy_zone(row(4), true).cy, 529.5);
        assert_eq!(buy_zone(row(5), true).cy, 674.5);
    }
}
