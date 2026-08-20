//! When each act lands: the tuned per-animation baselines, the player's
//! extra-wait ranges on top of them, and the presets that dial those ranges.
//!
//! The public half of `plan` — `config`, `ui::editor` and its `timing_meter`
//! all speak [`Timings`], [`DelayRange`] and the `WAIT_*_MS` baselines, and
//! `timing_meter` `const _: () = assert!`s against seven of the eight.

use serde::{Deserialize, Serialize};

use super::jitter::Jitter;

// Block-animation waits (dispatch margin included): the game ignores input while
// the matching animation runs, so every step waits before it acts. Public
// because the Setup editor shows each as the baseline its range adds onto.
pub const WAIT_SHOP_OPENED_MS: u64 = 1_180;
pub const WAIT_REFRESHED_MS: u64 = 780;
pub const WAIT_PURCHASE_RESUMED_MS: u64 = 400;
/// A watchdog retry fires into an idle game (the awaited animation never
/// played), so this is dispatch margin only.
pub const WAIT_RECOVERY_MS: u64 = 400;
pub const WAIT_CONFIRM_REFRESH_MODAL_MS: u64 = 270;
pub const WAIT_BUY_MODAL_MS: u64 = 150;
pub const WAIT_BETWEEN_BUYS_MS: u64 = 600;
/// A wheel scroll blocks nothing: only input-dispatch time before the click.
pub const WAIT_SCROLL_SETTLE_MS: u64 = 100;

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
    pub const fn pre_wait_ms(self) -> u64 {
        match self {
            Trigger::ShopOpened => WAIT_SHOP_OPENED_MS,
            Trigger::Refreshed => WAIT_REFRESHED_MS,
            Trigger::PurchaseResumed => WAIT_PURCHASE_RESUMED_MS,
            Trigger::Recovery => WAIT_RECOVERY_MS,
        }
    }
}

/// The ceiling on a single extra wait: one minute — fifty times the slowest
/// baseline (1180 ms, `shop_opened`) and twenty-four times what the Setup tab's
/// 2500 ms meter can produce, so every human-pace setting stays reachable. What
/// it makes unreachable: a `max_ms` in the tens of minutes, which freezes the
/// loop between two clicks indistinguishably from a hang, and a value near
/// `u64::MAX`, which overflows the editor's plain `baseline + extra` sums.
pub const MAX_TIMING_MS: u64 = 60_000;

/// Why a `(min_ms, max_ms)` pair is not a [`DelayRange`].
///
/// Both messages say what the value *would have done*: this is the text a player
/// reads in an error window over a file they are told not to hand-edit. Neither
/// names the key — `toml` prefixes the failing key's line and span, and a struct
/// literal is named by the compiler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelayRangeError {
    /// `min_ms > max_ms`. In the inline TOML form this table uses
    /// (`{ min_ms = 800, max_ms = 200 }`) swapping the two is an ordinary typo,
    /// and read leniently it becomes a fixed 800 ms delay — variability
    /// configured, none delivered, and the Setup tab showing "Custom".
    Reversed { min_ms: u64, max_ms: u64 },
    /// `max_ms` past [`MAX_TIMING_MS`].
    AboveCeiling { max_ms: u64 },
}

impl std::fmt::Display for DelayRangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            DelayRangeError::Reversed { min_ms, max_ms } => write!(
                f,
                "the range is reversed: min_ms = {min_ms} is above max_ms = {max_ms} — swap them (it would be read as a fixed {min_ms} ms delay, not a range)"
            ),
            DelayRangeError::AboveCeiling { max_ms } => write!(
                f,
                "max_ms = {max_ms} exceeds the {MAX_TIMING_MS} ms ceiling — that would stall the refresh loop between two clicks"
            ),
        }
    }
}

impl std::error::Error for DelayRangeError {}

/// The wire shape of a [`DelayRange`], and only that: deriving `Deserialize` on
/// the newtype would let serde fill the private fields and skip the check.
///
/// Both keys stay `Option` — do not put the flat `#[serde(default)]` back.
/// `refreshed = { min_ms = 200 }` is legal and ordinary, and under the default it
/// deserialized as `(200, 0)`, failed as [`DelayRangeError::Reversed`] advising a
/// swap the player never wrote, after which `config::parse`'s salvage dropped the
/// range to `0..=0` and the line silently stopped applying.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawDelayRange {
    min_ms: Option<u64>,
    max_ms: Option<u64>,
}

impl TryFrom<RawDelayRange> for DelayRange {
    type Error = DelayRangeError;

    fn try_from(raw: RawDelayRange) -> Result<Self, Self::Error> {
        let min_ms = raw.min_ms.unwrap_or(0);
        // A floor with no ceiling is the fixed delay `min..=min`; a ceiling with
        // no floor keeps the `0`, which is `DelayRange::ceiling`'s shape.
        // Neither absence can reverse the range, so only a pair the player did
        // write backwards reaches the error.
        let max_ms = raw.max_ms.unwrap_or(min_ms);
        DelayRange::try_new(min_ms, max_ms)
    }
}

/// An inclusive extra-wait range, in milliseconds, drawn uniformly and added to
/// a tuned baseline so the loop's pauses vary like a human's instead of being
/// byte-identical. The baseline is the floor, so a range only slows the loop
/// down; the default (`0..=0`) reproduces the calibrated timing exactly.
///
/// `min_ms <= max_ms <= MAX_TIMING_MS` holds **by construction**. Do not move
/// the check back into `config::validate_timings` and let each producer (a
/// preset, a GUI drag, `persist::save`) re-derive or bypass it — a GUI write one
/// missing clamp from invalid is a file the next launch refuses, recoverable
/// only by hand-editing a file the app owns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "RawDelayRange")]
pub struct DelayRange {
    min_ms: u64,
    max_ms: u64,
}

impl DelayRange {
    /// # Errors
    ///
    /// The check order matters for the message a player sees: a reversed pair is
    /// [`DelayRangeError::Reversed`] even when it also breaks the
    /// [`MAX_TIMING_MS`] ceiling, because swapping it is the fix.
    pub const fn try_new(min_ms: u64, max_ms: u64) -> Result<Self, DelayRangeError> {
        if min_ms > max_ms {
            return Err(DelayRangeError::Reversed { min_ms, max_ms });
        }
        if max_ms > MAX_TIMING_MS {
            return Err(DelayRangeError::AboveCeiling { max_ms });
        }
        Ok(Self { min_ms, max_ms })
    }

    /// A range with no floor — `0..=max_ms`, clamped to [`MAX_TIMING_MS`].
    ///
    /// Infallible, and that is the point: this is the shape both in-app
    /// producers make (a preset dials the ceiling, the Setup tab's drag sets
    /// it), so neither needs a `Result` it could only `expect` on.
    #[must_use]
    pub const fn ceiling(max_ms: u64) -> Self {
        Self {
            min_ms: 0,
            max_ms: if max_ms > MAX_TIMING_MS {
                MAX_TIMING_MS
            } else {
                max_ms
            },
        }
    }

    /// The floor of the draw: extra wait always added.
    #[must_use]
    pub const fn min_ms(self) -> u64 {
        self.min_ms
    }

    /// At most [`MAX_TIMING_MS`], never below [`min_ms`](Self::min_ms).
    #[must_use]
    pub const fn max_ms(self) -> u64 {
        self.max_ms
    }

    /// Keeps the invariant across a drag: clamped to [`MAX_TIMING_MS`], and a
    /// config-seeded floor above the new ceiling comes down with it rather than
    /// leaving a reversed range.
    pub const fn set_max_ms(&mut self, max_ms: u64) {
        self.max_ms = if max_ms > MAX_TIMING_MS {
            MAX_TIMING_MS
        } else {
            max_ms
        };
        if self.min_ms > self.max_ms {
            self.min_ms = self.max_ms;
        }
    }

    /// The inert default (`0..=0`): calibrated baseline, no extra wait.
    /// Persistence skips these so a first Apply does not fill
    /// `[actuator.timings]` with eight no-op ranges the player never set.
    pub fn is_inert(&self) -> bool {
        self.min_ms == 0 && self.max_ms == 0
    }

    /// A uniform draw in `[min_ms, max_ms]`. Plain arithmetic because the
    /// invariant makes it provable: the span cannot underflow, and the inclusive
    /// `span + 1` modulus cannot overflow — which used to make `% 0` reachable
    /// from a `max_ms = u64::MAX` config file.
    fn draw(&self, jitter: &mut Jitter) -> u64 {
        let span = self.max_ms - self.min_ms;
        if span == 0 {
            return self.min_ms;
        }
        self.min_ms + jitter.next() % (span + 1)
    }
}

/// Player-set extra-wait ranges, added on top of every tuned baseline above.
///
/// Serialization skips every inert range, because `config::persist` replaces the
/// whole `[actuator.timings]` table on each Apply and eight
/// `{ min_ms = 0, max_ms = 0 }` lines the player never asked for would fight that
/// module's purpose (preserving the shape of a hand-authored file). Whole ranges
/// only — never a lone `min_ms = 0` inside a range that *is* written, where the
/// zero is the draw's floor and the pair is the style the example documents.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Timings {
    /// Before the first click once the shop opens.
    #[serde(skip_serializing_if = "DelayRange::is_inert")]
    pub shop_opened: DelayRange,
    /// Before the first click after a paid refresh.
    #[serde(skip_serializing_if = "DelayRange::is_inert")]
    pub refreshed: DelayRange,
    /// Before the first click when resuming after a purchase.
    #[serde(skip_serializing_if = "DelayRange::is_inert")]
    pub purchase_resumed: DelayRange,
    /// Before a watchdog re-issue (the game sits idle).
    #[serde(skip_serializing_if = "DelayRange::is_inert")]
    pub recovery: DelayRange,
    /// Between the Refresh click and its confirm click.
    #[serde(skip_serializing_if = "DelayRange::is_inert")]
    pub confirm_refresh_modal: DelayRange,
    /// Between a Buy click and its confirm click.
    #[serde(skip_serializing_if = "DelayRange::is_inert")]
    pub buy_modal: DelayRange,
    /// Between two consecutive buys.
    #[serde(skip_serializing_if = "DelayRange::is_inert")]
    pub between_buys: DelayRange,
    /// After a wheel scroll before the next click.
    #[serde(skip_serializing_if = "DelayRange::is_inert")]
    pub scroll_settle: DelayRange,
}

/// Eight 16-byte ranges, `Copy` deliberately above the usual 64-byte guidance:
/// no heap data, built a few times per refresh rather than per packet, and the
/// alternative forces a `.clone()` signalling a cost there is none of. The
/// canary makes a ninth action a decision, not a surprise.
const _: () = assert!(size_of::<Timings>() == 128);

impl Timings {
    /// Every range paired with its `[actuator.timings]` key — what
    /// `Config::validate` walks to bound the player's values. The destructuring
    /// is exhaustive on purpose: a ninth action stops compiling here until it is
    /// named, so validation cannot silently skip a knob that reaches the loop.
    pub fn named_ranges(&self) -> [(&'static str, DelayRange); 8] {
        let Timings {
            shop_opened,
            refreshed,
            purchase_resumed,
            recovery,
            confirm_refresh_modal,
            buy_modal,
            between_buys,
            scroll_settle,
        } = self;
        [
            ("shop_opened", *shop_opened),
            ("refreshed", *refreshed),
            ("purchase_resumed", *purchase_resumed),
            ("recovery", *recovery),
            ("confirm_refresh_modal", *confirm_refresh_modal),
            ("buy_modal", *buy_modal),
            ("between_buys", *between_buys),
            ("scroll_settle", *scroll_settle),
        ]
    }

    /// A trigger's tuned baseline plus a fresh draw from the matching range.
    pub(super) fn pre_wait_ms(&self, trigger: Trigger, jitter: &mut Jitter) -> u64 {
        let range = match trigger {
            Trigger::ShopOpened => self.shop_opened,
            Trigger::Refreshed => self.refreshed,
            Trigger::PurchaseResumed => self.purchase_resumed,
            Trigger::Recovery => self.recovery,
        };
        trigger.pre_wait_ms().saturating_add(range.draw(jitter))
    }

    pub(super) fn confirm_refresh_modal_ms(&self, jitter: &mut Jitter) -> u64 {
        WAIT_CONFIRM_REFRESH_MODAL_MS.saturating_add(self.confirm_refresh_modal.draw(jitter))
    }

    pub(super) fn buy_modal_ms(&self, jitter: &mut Jitter) -> u64 {
        WAIT_BUY_MODAL_MS.saturating_add(self.buy_modal.draw(jitter))
    }

    pub(super) fn between_buys_ms(&self, jitter: &mut Jitter) -> u64 {
        WAIT_BETWEEN_BUYS_MS.saturating_add(self.between_buys.draw(jitter))
    }

    pub(super) fn scroll_settle_ms(&self, jitter: &mut Jitter) -> u64 {
        WAIT_SCROLL_SETTLE_MS.saturating_add(self.scroll_settle.draw(jitter))
    }
}

/// One-touch humanization level: a named set of per-action random ceilings, so
/// the loop clicks less like a metronome. A preset dials only the `max_ms`;
/// `min_ms` stays a config-only floor, so it never rewrites the calibrated
/// minimum.
///
/// Every producer of a `Timings` *from* a preset must go through
/// [`applied_to`](Self::applied_to), the merge that keeps that promise. The
/// Setup tab used to assign [`timings`](Self::timings) wholesale, so a
/// `config.toml` floor was replaced by a range starting at 0 and `persist::save`
/// wrote the loss to disk — no warning, no undo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingPreset {
    /// Tuned minimums only, no random extra — `Timings::default()`.
    Instant,
    /// Modest random slack on the actions a watcher would notice.
    Human,
    /// Roughly double the slack: slowest and least regular.
    Cautious,
}

impl TimingPreset {
    /// Display order: fastest to most cautious.
    pub const ALL: [TimingPreset; 3] = [
        TimingPreset::Instant,
        TimingPreset::Human,
        TimingPreset::Cautious,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TimingPreset::Instant => "Instant",
            TimingPreset::Human => "Human",
            TimingPreset::Cautious => "Cautious",
        }
    }

    /// The level *on its own*: only `max_ms` is set, `min_ms` stays 0, and
    /// [`applied_to`](Self::applied_to) reads its ceilings out of this. Not what
    /// a player's click should produce — assigning it discards their floors.
    pub fn timings(self) -> Timings {
        // Per-action random ceilings (ms) for Human; Cautious doubles them. The
        // watchdog stays tight at every level — recovery is not humanization.
        let human = match self {
            TimingPreset::Instant => return Timings::default(),
            TimingPreset::Human => 1,
            TimingPreset::Cautious => 2,
        };
        let x = |base: u64| DelayRange::ceiling(base * human);
        Timings {
            shop_opened: x(500),
            refreshed: x(350),
            purchase_resumed: x(250),
            recovery: DelayRange::default(),
            confirm_refresh_modal: x(150),
            buy_modal: x(100),
            between_buys: x(400),
            scroll_settle: x(100),
        }
    }

    /// This level's ceilings over `current`'s floors — what choosing the level
    /// in the Setup tab produces. Every `min_ms` is the player's, kept; the only
    /// floor that moves is one the new ceiling dropped below, which
    /// [`DelayRange::set_max_ms`] brings down rather than leave reversed (a
    /// seeded `recovery` collapses to `0..=0`, since the watchdog has no random
    /// extra at any level).
    ///
    /// A merge here rather than at the one UI call site because the invariant
    /// belongs to the preset: a second caller forgetting to merge costs a
    /// setting on disk, not a compile error.
    #[must_use]
    pub fn applied_to(self, current: &Timings) -> Timings {
        let ceilings = self.timings();
        let mut merged = *current;
        // Exhaustive, for the reason `Timings::named_ranges` gives: a new knob
        // cannot quietly sit outside the preset control.
        let Timings {
            shop_opened,
            refreshed,
            purchase_resumed,
            recovery,
            confirm_refresh_modal,
            buy_modal,
            between_buys,
            scroll_settle,
        } = &mut merged;
        for (target, ceiling) in [
            (shop_opened, ceilings.shop_opened),
            (refreshed, ceilings.refreshed),
            (purchase_resumed, ceilings.purchase_resumed),
            (recovery, ceilings.recovery),
            (confirm_refresh_modal, ceilings.confirm_refresh_modal),
            (buy_modal, ceilings.buy_modal),
            (between_buys, ceilings.between_buys),
            (scroll_settle, ceilings.scroll_settle),
        ] {
            target.set_max_ms(ceiling.max_ms());
        }
        merged
    }

    /// The preset already in force in `timings`, or `None` when the player has
    /// fine-tuned away from every level ("Custom").
    ///
    /// "The level a click would not change", not `preset.timings() == *timings`:
    /// clicking a level on timings carrying a floor produces something equality
    /// could never match, so the segment just pressed would stay dark while the
    /// control had done exactly what it says. The merge asks what the label
    /// answers — are these ceilings this level's? — and a floor is orthogonal.
    ///
    /// [`applied_to`](Self::applied_to) is idempotent, so at most one level can
    /// match: `Instant` zeroes every ceiling and the other two do not.
    pub fn from_timings(timings: &Timings) -> Option<TimingPreset> {
        TimingPreset::ALL
            .into_iter()
            .find(|preset| preset.applied_to(timings) == *timings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actuator::plan::fixtures::range;

    #[test]
    fn instant_preset_is_the_default_timings() {
        assert_eq!(TimingPreset::Instant.timings(), Timings::default());
    }

    #[test]
    fn from_timings_round_trips_every_preset() {
        for preset in TimingPreset::ALL {
            assert_eq!(TimingPreset::from_timings(&preset.timings()), Some(preset));
        }
    }

    #[test]
    fn a_fine_tuned_timings_matches_no_preset() {
        let mut custom = TimingPreset::Human.timings();
        custom.refreshed.set_max_ms(custom.refreshed.max_ms() + 5);
        assert_eq!(TimingPreset::from_timings(&custom), None);
    }

    #[test]
    fn presets_only_dial_the_random_ceiling() {
        // A preset must never write a floor: the minimum is config-only.
        for preset in TimingPreset::ALL {
            let t = preset.timings();
            for range in [
                t.shop_opened,
                t.refreshed,
                t.purchase_resumed,
                t.recovery,
                t.confirm_refresh_modal,
                t.buy_modal,
                t.between_buys,
                t.scroll_settle,
            ] {
                assert_eq!(range.min_ms(), 0);
            }
        }
    }

    #[test]
    fn choosing_a_level_keeps_a_config_set_floor_and_takes_its_ceiling() {
        // The defect this merge exists for: one click on a preset wiped a floor
        // typed into config.toml, and `persist::save` wrote the zero to disk.
        let seeded = Timings {
            refreshed: range(200, 800),
            ..Timings::default()
        };
        let human = TimingPreset::Human.applied_to(&seeded);
        assert_eq!(human.refreshed.min_ms(), 200, "the player's floor stands");
        assert_eq!(
            human.refreshed.max_ms(),
            TimingPreset::Human.timings().refreshed.max_ms(),
            "and the level's ceiling replaced theirs"
        );
        assert_eq!(
            human.between_buys,
            TimingPreset::Human.timings().between_buys
        );
        // The control lights the level the player just chose, floor or not —
        // the whole reason `from_timings` compares against the merge.
        assert_eq!(
            TimingPreset::from_timings(&human),
            Some(TimingPreset::Human)
        );
    }

    #[test]
    fn a_floor_above_the_chosen_ceiling_comes_down_with_it() {
        // The one case where a level *does* move a floor, for `set_max_ms`'s
        // reason and not the preset's: `recovery` has no random extra at any
        // level, and `min_ms = 100, max_ms = 0` is not a range this type holds.
        let seeded = Timings {
            recovery: range(100, 200),
            buy_modal: range(40, 60),
            ..Timings::default()
        };
        let cautious = TimingPreset::Cautious.applied_to(&seeded);
        assert_eq!(
            (cautious.recovery.min_ms(), cautious.recovery.max_ms()),
            (0, 0)
        );
        assert_eq!(cautious.buy_modal.min_ms(), 40, "under the ceiling, kept");
        assert_eq!(
            TimingPreset::from_timings(&cautious),
            Some(TimingPreset::Cautious)
        );
    }

    #[test]
    fn applying_a_level_twice_changes_nothing_the_second_time() {
        // What lets `from_timings` be "the level a click would not change":
        // without idempotence the detected level flickers frame to frame.
        let seeded = Timings {
            refreshed: range(200, 800),
            scroll_settle: range(10, 10),
            ..Timings::default()
        };
        for preset in TimingPreset::ALL {
            let once = preset.applied_to(&seeded);
            assert_eq!(preset.applied_to(&once), once, "{}", preset.label());
        }
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
    fn a_reversed_range_cannot_be_built_at_all() {
        // The message says what the value would have been read as, because that
        // is what tells the player it was a typo.
        let err = DelayRange::try_new(600, 100).expect_err("a reversed range is not a range");
        assert_eq!(
            err,
            DelayRangeError::Reversed {
                min_ms: 600,
                max_ms: 100
            }
        );
        let message = err.to_string();
        assert!(
            message.contains("600") && message.contains("100"),
            "{message}"
        );
        assert!(message.contains("fixed 600 ms delay"), "{message}");
    }

    #[test]
    fn one_bound_written_alone_is_a_range_and_not_a_reversed_pair() {
        // `{ min_ms = 200 }` is ordinary to write and used to read as `(200, 0)`
        // — `Reversed`, advising a swap for a mistake the player did not make,
        // after which the salvage dropped the range to `0..=0` and the line
        // silently stopped applying.
        let floor_only: DelayRange =
            toml::from_str("min_ms = 200").expect("a floor alone is a fixed delay");
        assert_eq!(floor_only, DelayRange::try_new(200, 200).expect("legal"));
        // The mirror case keeps the `0` floor: `DelayRange::ceiling`'s shape.
        let ceiling_only: DelayRange =
            toml::from_str("max_ms = 400").expect("a ceiling alone is a 0..=max range");
        assert_eq!(ceiling_only, DelayRange::ceiling(400));
        assert_eq!(
            toml::from_str::<DelayRange>("").expect("an absent table is the default"),
            DelayRange::default()
        );
        assert!(toml::from_str::<DelayRange>("min_ms = 600\nmax_ms = 100").is_err());
    }

    #[test]
    fn a_range_past_the_ceiling_cannot_be_built_and_the_ceiling_itself_can() {
        // `u64::MAX` would overflow `draw`'s modulus and the editor's
        // `baseline + max` sums; 600_000 (ten minutes) would freeze the loop
        // between two clicks. The inclusive bound itself stays usable.
        assert_eq!(
            DelayRange::try_new(0, u64::MAX),
            Err(DelayRangeError::AboveCeiling { max_ms: u64::MAX })
        );
        assert_eq!(
            DelayRange::try_new(0, 600_000),
            Err(DelayRangeError::AboveCeiling { max_ms: 600_000 })
        );
        assert_eq!(range(0, MAX_TIMING_MS).max_ms(), MAX_TIMING_MS);
        assert_eq!(DelayRange::ceiling(u64::MAX).max_ms(), MAX_TIMING_MS);
        assert_eq!(DelayRange::ceiling(0), DelayRange::default());
    }

    #[test]
    fn set_max_ms_keeps_the_invariant_it_could_break() {
        // The Setup tab's drag is the one mutating producer, and dragging below
        // a config-seeded floor must bring the floor down rather than leave a
        // reversed range — the check `timing_meter` used to make by hand.
        let mut r = range(400, 900);
        r.set_max_ms(100);
        assert_eq!((r.min_ms(), r.max_ms()), (100, 100));
        r.set_max_ms(u64::MAX);
        assert_eq!((r.min_ms(), r.max_ms()), (100, MAX_TIMING_MS));
    }

    #[test]
    fn only_the_all_zero_range_is_inert() {
        // Drives the `skip_serializing_if` on every `Timings` field: a range
        // wrongly reported inert is dropped from a saved config.toml, reverting
        // the player's setting on the next launch.
        assert!(DelayRange::default().is_inert());
        assert!(range(0, 0).is_inert());
        assert!(!range(0, 1).is_inert());
        assert!(!range(1, 1).is_inert());
    }

    #[test]
    fn named_ranges_covers_every_field_under_its_config_key() {
        // Distinct values so a copy-paste in the pairing cannot pass.
        let timings = Timings {
            shop_opened: range(0, 1),
            refreshed: range(0, 2),
            purchase_resumed: range(0, 3),
            recovery: range(0, 4),
            confirm_refresh_modal: range(0, 5),
            buy_modal: range(0, 6),
            between_buys: range(0, 7),
            scroll_settle: range(0, 8),
        };
        let named = timings.named_ranges();
        assert_eq!(
            named.map(|(name, _)| name),
            [
                "shop_opened",
                "refreshed",
                "purchase_resumed",
                "recovery",
                "confirm_refresh_modal",
                "buy_modal",
                "between_buys",
                "scroll_settle",
            ]
        );
        assert_eq!(named.map(|(_, r)| r.max_ms()), [1, 2, 3, 4, 5, 6, 7, 8]);
    }
}
