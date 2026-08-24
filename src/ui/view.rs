//! Pure projection of the controller for the window: everything one frame
//! shows, copied under a single short lock. The hover tooltip
//! ([`slot_detail`]) and the slot rows ([`SlotRows`]) are deliberately not
//! copied per frame, so building a [`ViewState`] allocates nothing.

use super::editor::{hunt_summary, stop_summary};
use crate::domain::control::{Controller, Limits, Progress, Status};
use crate::domain::filter::Filter;
use crate::domain::shop::{CatalogId, Crystals, Gold, ShopItem};
use crate::render::{HAUL_HEADLINERS, format_item, haul_tally, kind_label, status_summary};

/// Every field is `Copy` or `&'static` bar [`ViewState::plan`], which is built
/// only while idle — so a frame of a live run copies this for free.
pub(super) struct ViewState {
    /// The state in one word — `Idle`, `Watching`, `Paused`, `Stopped`.
    pub status_word: &'static str,
    /// The clause beside the word: an invitation while idle, the stop reason
    /// once stopped, `None` while watching, where the word says it all.
    ///
    /// The two are carried apart rather than pre-joined because the two bands
    /// weight them differently — the idle band leads on the clause and buries
    /// the word, the run band writes them as one sentence. The console joins
    /// them its own way through `render::status_label`.
    pub status_hint: Option<&'static str>,
    /// Which band to draw and how to introduce the clause — not a colour, since
    /// no surface encodes state as a hue any more.
    pub status_kind: Status,
    /// Milliseconds since the run armed, `None` before the first `Start`.
    /// Feeds the duration dial and the refresh rate.
    pub elapsed_ms: Option<u64>,
    pub progress: Progress,
    pub limits: Limits,
    /// From the controller's enforced meta, not the raw snapshot.
    pub crystal_balance: Option<Crystals>,
    /// Last gold balance echoed by a purchase; `None` before the first buy and
    /// again after `Start`.
    pub gold_balance: Option<Gold>,
    /// A shop has been captured this session, even a degraded slotless one.
    /// Gates the welcome screen: empty rows alone must not resurrect it.
    pub has_snapshot: bool,
    /// What the run *would* do, for the band shown before one starts: the hunt
    /// criteria and the rails, in the words Setup's own folded summaries use.
    ///
    /// `None` once a run exists, which is what keeps the "allocates nothing"
    /// claim above honest where it matters: this is the projection's only
    /// allocation, and it is made in the one state where no figure is moving
    /// and nothing is being recomputed. It exists because the plan was
    /// otherwise unreadable without opening Setup.
    pub plan: Option<String>,
    /// Confirmed buys this run, per headline token, shown even at zero once a
    /// run exists.
    pub haul: [(&'static str, u32); HAUL_HEADLINERS.len()],
    /// Everything else bought this run, folded into one "+N other" bucket.
    pub haul_others: u32,
}

/// One shop slot as the table shows it.
pub(super) struct SlotRow {
    pub slot: u8,
    pub kind: &'static str,
    pub name: Option<String>,
    pub price: Option<Gold>,
    pub sold_out: bool,
    /// Matched and still to buy: the catalog id sits in the checklist.
    pub wanted: bool,
}

impl SlotRow {
    /// The cloned name is the only allocation a frame's projection makes —
    /// which is why [`SlotRows`] gates it.
    fn project(item: &ShopItem, index: usize, checklist: &[CatalogId]) -> Self {
        Self {
            slot: item.effective_slot(index),
            kind: kind_label(item.kind),
            name: item.name.clone(),
            price: item.price,
            sold_out: item.is_sold_out(),
            wanted: item.id.is_some_and(|id| checklist.contains(&id)),
        }
    }

    /// Whether this row still describes `item` at `index` — [`SlotRows`]'s
    /// gate, mirroring [`SlotRow::project`] with nothing allocated.
    ///
    /// `self` is destructured rather than read field by field so that a field
    /// added above but left out here becomes an unused binding, which CI denies.
    fn matches(&self, item: &ShopItem, index: usize, checklist: &[CatalogId]) -> bool {
        let Self {
            slot,
            kind,
            name,
            price,
            sold_out,
            wanted,
        } = self;
        *slot == item.effective_slot(index)
            && *kind == kind_label(item.kind)
            && name.as_deref() == item.name.as_deref()
            && *price == item.price
            && *sold_out == item.is_sold_out()
            && *wanted == item.id.is_some_and(|id| checklist.contains(&id))
    }
}

/// The slot table's rows, re-derived only when the shop or the checklist behind
/// them moved. This buys lock hold, not CPU: the projection runs inside the
/// controller lock the session loop needs, and hover lifts the repaint rate
/// above the 4 Hz idle poll.
///
/// The gate compares fields rather than a generation counter or `Arc::ptr_eq`:
/// [`Controller::last_snapshot`] stores the snapshot inline, so a replacement
/// lands at the same address — nothing cheaper is also correct.
#[derive(Default)]
pub(super) struct SlotRows(Vec<SlotRow>);

impl SlotRows {
    /// The cached rows, rendered after the controller guard is dropped.
    pub(super) fn rows(&self) -> &[SlotRow] {
        &self.0
    }

    /// Brings the cache up to date, re-deriving only when the projection moved.
    pub(super) fn sync(&mut self, controller: &Controller) {
        if self.is_current(controller) {
            return;
        }
        let checklist = controller.checklist();
        self.0 = controller
            .last_snapshot()
            .map_or_else(Vec::new, |snapshot| {
                snapshot
                    .slots
                    .iter()
                    .enumerate()
                    .map(|(index, item)| SlotRow::project(item, index, checklist))
                    .collect()
            });
    }

    /// Whether every cached row still describes its slot, with as many rows
    /// as slots. An absent snapshot projects to the empty slice.
    fn is_current(&self, controller: &Controller) -> bool {
        let slots = controller
            .last_snapshot()
            .map_or(&[][..], |snapshot| &snapshot.slots);
        if self.0.len() != slots.len() {
            return false;
        }
        let checklist = controller.checklist();
        self.0
            .iter()
            .zip(slots)
            .enumerate()
            .all(|(index, (row, item))| row.matches(item, index, checklist))
    }
}

/// The shop table's hover tooltip, built on demand rather than projected into
/// every [`SlotRow`]: as a field, `format_item` ran once per slot per frame
/// inside the controller lock, at hover's display repaint rate, for a string
/// only one row ever reads.
///
/// `index` is the row's position in the projected snapshot, so a message
/// landing between projection and hover describes the new roll — a sub-repaint
/// skew resolved on the next poll. Empty when that slot is gone.
pub(super) fn slot_detail(controller: &Controller, index: usize) -> String {
    controller
        .last_snapshot()
        .and_then(|snapshot| snapshot.slots.get(index))
        .map(|item| format_item(item, index))
        .unwrap_or_default()
}

/// Pure extraction, allocation-free: the caller holds the controller lock
/// only for this call.
///
/// `now_ms` comes from the session clock (`EventLog::now_ms`), the same one the
/// session stamps domain events with — so the elapsed time here and the
/// duration limit the controller enforces are measured against one clock and
/// cannot disagree.
///
/// The controller and the clock are the whole of what it takes. It used to carry
/// the server's vocabulary in as a third argument, purely so [`plan_summary`]
/// could name a token and a rarity; those words are relay constants now, read
/// where they are needed.
pub(super) fn view_state(controller: &Controller, now_ms: u64) -> ViewState {
    let (status_word, status_hint) = status_summary(controller);
    let (haul, haul_others) = haul_tally(controller.haul());
    ViewState {
        status_word,
        status_hint,
        status_kind: controller.status(),
        // Saturating: the window can render a frame stamped a hair before the
        // `Start` the session just handled, and a wrapped `u64` would read as
        // 584 million years of uptime.
        elapsed_ms: controller
            .started_at()
            .map(|start| now_ms.saturating_sub(start)),
        progress: controller.progress(),
        limits: *controller.limits(),
        crystal_balance: controller.refresh_meta().map(|meta| meta.crystal_balance),
        gold_balance: controller.gold_balance(),
        has_snapshot: controller.last_snapshot().is_some(),
        plan: matches!(controller.status(), Status::Idle)
            .then(|| plan_summary(controller.filter(), controller.limits())),
        haul,
        haul_others,
    }
}

/// What a run would hunt and what would stop it, in one line.
///
/// Built from Setup's own folded summaries rather than a second phrasing: the
/// idle band and the Hunt/Stop section bars would otherwise describe the same
/// two drafts in two vocabularies, and only one of them would get updated when
/// a criterion is added.
fn plan_summary(filter: &Filter, limits: &Limits) -> String {
    let rails = if has_any_limit(limits) {
        format!("stops at {}", stop_summary(limits))
    } else {
        "no limits set".to_owned()
    };
    if filter.is_unrestricted() {
        return format!("Nothing selected to hunt — {rails}");
    }
    format!("Hunting {} — {rails}", hunt_summary(filter))
}

/// Whether any rail is armed. Destructured rather than read field by field so
/// that a fifth limit added to [`Limits`] leaves an unused binding here, which
/// CI denies — the alternative is an idle band quietly reporting "no limits
/// set" over a rail that would stop the run.
fn has_any_limit(limits: &Limits) -> bool {
    let Limits {
        max_refreshes,
        max_spend,
        max_matches,
        max_duration_ms,
    } = limits;
    max_refreshes.is_some()
        || max_spend.is_some()
        || max_matches.is_some()
        || max_duration_ms.is_some()
}

/// Which limit the run will reach first. Ordered as [`Limits`] declares its
/// fields, which is also how ties break: two limits equally close leave the
/// earlier one winning, and it keeps winning at equal ratios, so the dial does
/// not flip between them frame to frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bound {
    Refreshes,
    Spend,
    Matches,
    Duration,
}

const BOUNDS: [Bound; 4] = [
    Bound::Refreshes,
    Bound::Spend,
    Bound::Matches,
    Bound::Duration,
];

/// The run's headline figure: the counter facing the limit closest to being
/// reached, with the cap it runs against.
pub(super) struct Dial {
    pub value: String,
    /// What the figure counts, for the caption beside it.
    pub caption: &'static str,
    /// The cap and how full it is — `None` when no limit is set at all. Both
    /// halves live in one `Option` because a gauge without a cap has nothing to
    /// fill towards: keeping them apart let a view paint a proportion of
    /// nothing.
    pub against: Option<Cap>,
}

pub(super) struct Cap {
    pub limit: String,
    /// `0.0..=1.0`.
    pub ratio: f32,
}

/// The run's headline figure, given what it has done and what would stop it.
///
/// The binding limit rather than a fixed one: [`Limits`] carries four, all
/// optional and independent, so pinning the dial to refreshes was a guess that
/// reads wrong the moment a different rail is the one about to fire. A gauge
/// that fills has to mean "this is about to stop", which is only true of the
/// nearest limit.
pub(super) fn dial(progress: Progress, limits: &Limits, elapsed_ms: Option<u64>) -> Dial {
    let elapsed = elapsed_ms.unwrap_or(0);
    let binding = BOUNDS
        .into_iter()
        .filter_map(|bound| Some((bound, fullness(bound, progress, limits, elapsed)?)))
        .reduce(|nearest, next| if next.1 > nearest.1 { next } else { nearest });
    let Some((bound, ratio)) = binding else {
        // Nothing bounds the run: the refresh count is the only figure that
        // means anything on its own, and the caller pairs it with a rate.
        return Dial {
            value: progress.refreshes.to_string(),
            caption: "refreshes",
            against: None,
        };
    };
    let (value, limit) = faces(bound, progress, limits, elapsed);
    Dial {
        value,
        caption: caption(bound),
        // `limit` is `Some` for any bound `fullness` scored, so the `None` arm
        // is unreachable — written as a fallback rather than an unwrap because
        // an unreachable arm that degrades is worth less than nothing when it
        // turns out to be reachable.
        against: limit.map(|limit| Cap { limit, ratio }),
    }
}

/// How far this limit has been consumed, or `None` when it is not set.
fn fullness(bound: Bound, progress: Progress, limits: &Limits, elapsed: u64) -> Option<f32> {
    match bound {
        Bound::Refreshes => limits
            .max_refreshes
            .map(|cap| share(progress.refreshes, cap)),
        // Through the currency's own method: `Crystals::get` documents itself
        // as not-for-arithmetic, and a ratio is arithmetic.
        Bound::Spend => limits.max_spend.map(|cap| progress.spent.ratio_of(cap)),
        Bound::Matches => limits
            .max_matches
            .map(|cap| share(progress.matches_found, cap)),
        Bound::Duration => limits.max_duration_ms.map(|cap| share_ms(elapsed, cap)),
    }
}

/// The two numbers this bound puts on screen: what has been done, and the cap.
fn faces(
    bound: Bound,
    progress: Progress,
    limits: &Limits,
    elapsed: u64,
) -> (String, Option<String>) {
    match bound {
        Bound::Refreshes => (
            progress.refreshes.to_string(),
            limits.max_refreshes.map(|cap| cap.to_string()),
        ),
        Bound::Spend => (
            progress.spent.to_string(),
            limits.max_spend.map(|cap| cap.to_string()),
        ),
        Bound::Matches => (
            progress.matches_found.to_string(),
            limits.max_matches.map(|cap| cap.to_string()),
        ),
        // Whole minutes, rounded the way the Setup summary rounds them
        // (`stop_summary`), so the cap the player set there is the number they
        // read back here.
        Bound::Duration => (
            (elapsed / 60_000).to_string(),
            limits
                .max_duration_ms
                .map(|cap| cap.div_ceil(60_000).to_string()),
        ),
    }
}

/// The game's word for each rail, not the field's. `max_spend` is crystals in
/// the domain and Skystones on screen.
fn caption(bound: Bound) -> &'static str {
    match bound {
        Bound::Refreshes => "refreshes",
        Bound::Spend => "skystones spent",
        Bound::Matches => "matches found",
        Bound::Duration => "minutes",
    }
}

/// A gauge fill, bounded at both ends. A cap of zero reads as reached rather
/// than dividing by it.
fn share(value: u32, cap: u32) -> f32 {
    if cap == 0 {
        return 1.0;
    }
    (value as f32 / cap as f32).clamp(0.0, 1.0)
}

/// [`share`] for the millisecond rail, whose numbers outgrow `u32`.
fn share_ms(value: u64, cap: u64) -> f32 {
    if cap == 0 {
        return 1.0;
    }
    (value as f64 / cap as f64).clamp(0.0, 1.0) as f32
}

/// Refreshes per minute — what the dial shows beside the count when no limit
/// bounds the run, so an unbounded run still has something that moves.
///
/// `None` under half a minute: three refreshes in four seconds divides out to a
/// rate in the hundreds, which is arithmetic rather than information.
pub(super) fn refresh_rate(refreshes: u32, elapsed_ms: Option<u64>) -> Option<String> {
    const FLOOR_MS: u64 = 30_000;
    let elapsed = elapsed_ms.filter(|ms| *ms >= FLOOR_MS)?;
    let per_minute = f64::from(refreshes) * 60_000.0 / elapsed as f64;
    // A decimal under ten, because the difference between 6 and 7 a minute is
    // the difference between a healthy loop and one losing a click in six.
    Some(if per_minute >= 10.0 {
        format!("{per_minute:.0} / min")
    } else {
        format!("{per_minute:.1} / min")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::control::{Event, StopReason};
    use crate::domain::filter::Filter;
    use crate::domain::shop::{PurchaseLimit, RefreshMeta, ShopItem, ShopSnapshot};

    fn controller() -> Controller {
        Controller::new(Filter::matching_default_items(), Limits::default())
    }

    fn shop(slots: Vec<ShopItem>) -> ShopSnapshot {
        ShopSnapshot {
            merchant: None,
            slots,
            refresh: None,
        }
    }

    fn rows(controller: &Controller) -> SlotRows {
        let mut rows = SlotRows::default();
        rows.sync(controller);
        rows
    }

    #[test]
    fn view_state_on_fresh_controller_is_idle_and_empty() {
        let ctrl = controller();
        let view = view_state(&ctrl, 0);
        assert_eq!(view.status_hint, Some("ready to start"));
        assert_eq!(view.status_kind, Status::Idle);
        assert!(!view.has_snapshot);
        assert!(rows(&ctrl).rows().is_empty());
        assert_eq!(view.crystal_balance, None);
        assert_eq!(view.gold_balance, None);
        // No run has armed, so there is no clock to read against.
        assert_eq!(view.elapsed_ms, None);
    }

    /// The elapsed time is a subtraction against the *session* clock, not a
    /// stored duration: the window passes whatever `EventLog::now_ms` says.
    #[test]
    fn elapsed_time_runs_from_the_start_event() {
        let mut ctrl = controller();
        let _ = ctrl.handle(Event::Start { now_ms: 4_000 });
        assert_eq!(view_state(&ctrl, 64_000).elapsed_ms, Some(60_000));
        // A frame stamped before the `Start` the session just handled floors at
        // zero rather than wrapping into geological time.
        assert_eq!(view_state(&ctrl, 3_999).elapsed_ms, Some(0));
    }

    /// The idle band says what a run would hunt, and it says it in words.
    ///
    /// Both criteria that have any are checked: the token — `friendpoint_name`,
    /// the one `render::HAUL_HEADLINERS` cannot name, which is how the raw id
    /// used to reach the top bar — and the rarity floor, which must read `Epic+`
    /// and never the ordinal. Asserted here rather than only in `hunt_summary`'s
    /// own tests because this is the projection that has to reach for them.
    #[test]
    fn the_idle_plan_names_its_criteria_in_words() {
        let hunting = Controller::new(
            Filter {
                names: vec!["friendpoint_name".to_owned()],
                min_grade: Some(5),
                ..Filter::default()
            },
            Limits::default(),
        );
        let plan = view_state(&hunting, 0)
            .plan
            .expect("an idle controller states its plan");
        assert!(plan.contains("Friendship Points"), "{plan}");
        assert!(plan.contains("Epic+"), "{plan}");
    }

    #[test]
    fn slot_rows_use_effective_slot_fallback() {
        let mut ctrl = controller();
        // The second slot carries no wire slot and falls back to its 1-based
        // position.
        let slots = vec![
            ShopItem {
                slot: 5,
                ..ShopItem::default()
            },
            ShopItem {
                slot: 0,
                ..ShopItem::default()
            },
        ];
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: shop(slots),
            now_ms: 0,
        });
        let rows = rows(&ctrl);
        assert_eq!(rows.rows()[0].slot, 5);
        assert_eq!(rows.rows()[1].slot, 2);
    }

    #[test]
    fn slot_rows_flag_checklist_rows_as_wanted() {
        let mut ctrl = controller();
        let _ = ctrl.handle(Event::Start { now_ms: 0 });
        // The default filter matches both, but only a trackable id enters the
        // checklist: the id-0 sentinel row must never read as wanted.
        let slots = vec![
            ShopItem {
                id: CatalogId::new(42),
                ..ShopItem::default()
            },
            ShopItem {
                id: CatalogId::new(0),
                ..ShopItem::default()
            },
        ];
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: shop(slots),
            now_ms: 1,
        });
        let rows = rows(&ctrl);
        assert!(rows.rows()[0].wanted);
        assert!(!rows.rows()[1].wanted);
    }

    #[test]
    fn slot_rows_flag_sold_out_rows() {
        let mut ctrl = controller();
        let slots = vec![ShopItem {
            limit: Some(PurchaseLimit {
                remaining: 0,
                total: 1,
            }),
            ..ShopItem::default()
        }];
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: shop(slots),
            now_ms: 0,
        });
        assert!(rows(&ctrl).rows()[0].sold_out);
    }

    #[test]
    fn view_state_copies_refresh_meta_when_present() {
        let mut ctrl = controller();
        let snapshot = ShopSnapshot {
            merchant: Some("Secret Shop".to_owned()),
            slots: vec![ShopItem::default()],
            refresh: Some(RefreshMeta {
                crystal_balance: Crystals::new(95),
                cost: Crystals::new(3),
            }),
        };
        let _ = ctrl.handle(Event::Snapshot {
            snapshot,
            now_ms: 0,
        });
        let view = view_state(&ctrl, 0);
        assert_eq!(view.crystal_balance, Some(Crystals::new(95)));
    }

    #[test]
    fn view_state_balance_survives_meta_less_snapshot() {
        let mut ctrl = controller();
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: ShopSnapshot {
                merchant: None,
                slots: vec![ShopItem::default()],
                refresh: Some(RefreshMeta {
                    crystal_balance: Crystals::new(95),
                    cost: Crystals::new(3),
                }),
            },
            now_ms: 0,
        });
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: shop(vec![ShopItem::default()]),
            now_ms: 1,
        });
        assert_eq!(
            view_state(&ctrl, 0).crystal_balance,
            Some(Crystals::new(95))
        );
    }

    #[test]
    fn view_state_balance_cleared_on_restart() {
        // `Start` discards a stale balance, and it must not resurrect from the
        // stored snapshot.
        let mut ctrl = controller();
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: ShopSnapshot {
                merchant: None,
                slots: vec![ShopItem::default()],
                refresh: Some(RefreshMeta {
                    crystal_balance: Crystals::new(95),
                    cost: Crystals::new(3),
                }),
            },
            now_ms: 0,
        });
        let _ = ctrl.handle(Event::Start { now_ms: 1 });
        assert_eq!(view_state(&ctrl, 0).crystal_balance, None);
    }

    #[test]
    fn view_state_surfaces_gold_balance_from_a_purchase() {
        let mut ctrl = controller();
        assert_eq!(view_state(&ctrl, 0).gold_balance, None);
        let _ = ctrl.handle(Event::Purchase {
            item: CatalogId::new(42),
            gold: Some(Gold::new(1_204_000)),
            now_ms: 0,
        });
        assert_eq!(
            view_state(&ctrl, 0).gold_balance,
            Some(Gold::new(1_204_000))
        );
    }

    #[test]
    fn view_state_reports_stop_reason_when_stopped() {
        let mut ctrl = controller();
        let _ = ctrl.handle(Event::Start { now_ms: 0 });
        let _ = ctrl.handle(Event::Stop);
        let view = view_state(&ctrl, 0);
        assert_eq!(view.status_kind, Status::Stopped(StopReason::PlayerStopped));
        // The stop reason rides in the hint, not a separate field.
        assert_eq!(view.status_hint, Some("at your request"));
    }

    fn progress(refreshes: u32, spent: u32, matches_found: u32) -> Progress {
        Progress {
            refreshes,
            spent: Crystals::new(spent),
            matches_found,
        }
    }

    /// With nothing set, there is no proportion to paint — and the figure falls
    /// back to the one count that means something unbounded.
    #[test]
    fn an_unbounded_run_has_a_count_and_no_cap() {
        let dial = dial(progress(42, 126, 3), &Limits::default(), Some(360_000));
        assert_eq!(dial.value, "42");
        assert_eq!(dial.caption, "refreshes");
        assert!(dial.against.is_none());
    }

    /// The whole point of picking a bound rather than fixing one: with four
    /// rails armed, the dial has to show the one that will fire, which here is
    /// the crystal budget at 84% against refreshes at 70%.
    #[test]
    fn the_dial_shows_the_limit_that_will_stop_the_run() {
        let limits = Limits {
            max_refreshes: Some(60),
            max_spend: Some(Crystals::new(150)),
            max_matches: Some(5),
            max_duration_ms: Some(20 * 60_000),
        };
        let dial = dial(progress(42, 126, 3), &limits, Some(6 * 60_000));
        assert_eq!(dial.caption, "skystones spent");
        assert_eq!(dial.value, "126");
        let cap = dial.against.expect("a bound run fills a gauge");
        assert_eq!(cap.limit, "150");
        assert!((cap.ratio - 0.84).abs() < 0.001, "ratio was {}", cap.ratio);
    }

    /// Same rails, a run that spent little and refreshed a lot: the dial has to
    /// follow, or the gauge stops meaning "about to stop".
    #[test]
    fn the_binding_limit_is_not_always_the_same_one() {
        let limits = Limits {
            max_refreshes: Some(60),
            max_spend: Some(Crystals::new(150)),
            ..Limits::default()
        };
        let dial = dial(progress(57, 12, 0), &limits, Some(6 * 60_000));
        assert_eq!(dial.caption, "refreshes");
        assert_eq!(dial.value, "57");
    }

    /// The duration rail counts whole minutes, rounded the way the Setup
    /// summary rounds them, so the cap reads back as the number that was set.
    #[test]
    fn the_duration_dial_reads_in_minutes() {
        let limits = Limits {
            max_duration_ms: Some(90_000),
            ..Limits::default()
        };
        let dial = dial(progress(0, 0, 0), &limits, Some(45_000));
        assert_eq!(dial.caption, "minutes");
        assert_eq!(dial.value, "0", "45 seconds is not yet a minute");
        let cap = dial.against.expect("a duration rail fills a gauge");
        assert_eq!(cap.limit, "2", "90s rounds up, as `stop_summary` does");
        assert!((cap.ratio - 0.5).abs() < 0.001);
    }

    /// A limit already exceeded — a budget lowered mid-run — must not paint
    /// past the panel.
    #[test]
    fn an_exceeded_limit_fills_the_gauge_and_stops_there() {
        let limits = Limits {
            max_refreshes: Some(10),
            ..Limits::default()
        };
        let cap = dial(progress(400, 0, 0), &limits, None)
            .against
            .expect("a bound run fills a gauge");
        assert_eq!(cap.ratio, 1.0);
    }

    #[test]
    fn a_refresh_rate_needs_enough_run_to_divide_by() {
        // Four seconds in, the division is arithmetic rather than information.
        assert_eq!(refresh_rate(3, Some(4_000)), None);
        assert_eq!(refresh_rate(3, None), None);
        // Six minutes, 42 refreshes: seven a minute, and the decimal is kept
        // even when it is a zero — a rate that gains and loses a digit as it
        // crosses a whole number reads as a glitch.
        assert_eq!(
            refresh_rate(42, Some(360_000)).as_deref(),
            Some("7.0 / min")
        );
        // Under ten a minute the decimal is kept — the gap between 6 and 7 is
        // the gap between a healthy loop and one dropping a click in six.
        assert_eq!(
            refresh_rate(40, Some(360_000)).as_deref(),
            Some("6.7 / min")
        );
    }

    #[test]
    fn slot_detail_matches_format_item() {
        let mut ctrl = controller();
        let item = ShopItem {
            id: CatalogId::new(7),
            slot: 3,
            name: Some("Covenant Bookmark".to_owned()),
            price: Some(Gold::new(184_000)),
            ..ShopItem::default()
        };
        let expected = format_item(&item, 0);
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: shop(vec![item]),
            now_ms: 0,
        });
        assert_eq!(slot_detail(&ctrl, 0), expected);
    }

    #[test]
    fn slot_detail_of_a_vanished_slot_is_empty() {
        // The index may no longer exist by the time this is called.
        let mut ctrl = controller();
        assert_eq!(slot_detail(&ctrl, 0), "");
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: shop(vec![ShopItem::default()]),
            now_ms: 0,
        });
        assert_eq!(slot_detail(&ctrl, 3), "");
    }

    /// Asserted through capacity, not identity: `sync` `collect`s an iterator
    /// of known length, so a rebuilt vector's capacity equals its length, and
    /// growing the live one first tells the two apart. `as_ptr` would prove
    /// nothing — a freed buffer can come back at the same address.
    #[test]
    fn slot_rows_are_not_re_derived_while_the_shop_and_the_checklist_hold() {
        let mut ctrl = controller();
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: shop(vec![ShopItem {
                name: Some("Covenant Bookmark".to_owned()),
                price: Some(Gold::new(184_000)),
                ..ShopItem::default()
            }]),
            now_ms: 0,
        });
        let mut rows = SlotRows::default();
        rows.sync(&ctrl);
        assert_eq!(rows.rows().len(), 1);
        rows.0.reserve(64);
        let capacity = rows.0.capacity();

        // Repaints with nothing new to show.
        for _ in 0..10 {
            rows.sync(&ctrl);
        }

        assert_eq!(
            rows.0.capacity(),
            capacity,
            "the rows were re-derived on a frame that had no reason to"
        );
        assert!(rows.is_current(&ctrl));
    }

    #[test]
    fn slot_rows_are_re_derived_when_the_shop_rolls_over() {
        let mut ctrl = controller();
        let roll = |name: &str| {
            shop(vec![ShopItem {
                name: Some(name.to_owned()),
                ..ShopItem::default()
            }])
        };
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: roll("Covenant Bookmark"),
            now_ms: 0,
        });
        let mut rows = SlotRows::default();
        rows.sync(&ctrl);

        // A new roll lands at the same address, and the gate must notice.
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: roll("Mystic Medal"),
            now_ms: 1,
        });
        assert!(!rows.is_current(&ctrl));
        rows.sync(&ctrl);
        assert_eq!(rows.rows()[0].name.as_deref(), Some("Mystic Medal"));
    }

    #[test]
    fn slot_rows_are_re_derived_when_the_checklist_moves_under_them() {
        // `wanted` is not in the snapshot, so keying on the shop alone would
        // leave the row green after the purchase.
        let mut ctrl = controller();
        let id = CatalogId::new(42);
        let _ = ctrl.handle(Event::Start { now_ms: 0 });
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: shop(vec![ShopItem {
                id,
                ..ShopItem::default()
            }]),
            now_ms: 1,
        });
        let mut rows = SlotRows::default();
        rows.sync(&ctrl);
        assert!(rows.rows()[0].wanted);

        let _ = ctrl.handle(Event::Purchase {
            item: id,
            gold: None,
            now_ms: 2,
        });

        assert!(!rows.is_current(&ctrl));
        rows.sync(&ctrl);
        assert!(!rows.rows()[0].wanted);
    }
}
