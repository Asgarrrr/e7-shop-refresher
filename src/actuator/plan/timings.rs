//! When each act lands: the tuned per-animation baselines, the player's
//! extra-wait ranges on top of them, and the presets that dial those ranges.
//!
//! This is the genuinely public half of `plan`, and the reason the seam is here
//! rather than only between geometry and jobs: `config`, `ui::editor` and
//! `ui::editor::timing_meter` all speak [`Timings`], [`DelayRange`] and the
//! `WAIT_*_MS` baselines, and `timing_meter` `const _: () = assert!`s against
//! seven of the eight baselines. One file makes that cross-module contract a
//! file boundary instead of a per-item annotation.
//!
//! Reads [`Jitter`] — a resolved wait is a baseline plus a draw — and nothing
//! else in the module.

use serde::{Deserialize, Serialize};

use super::jitter::Jitter;

// Block-animation waits (dispatch margin included): the game ignores input
// while the matching animation runs, so every step waits before it acts.
// Public because the Setup editor shows each as the baseline its extra-delay
// range adds onto — one source of truth, no hand-copied hints.
pub const WAIT_SHOP_OPENED_MS: u64 = 1_180;
pub const WAIT_REFRESHED_MS: u64 = 780;
pub const WAIT_PURCHASE_RESUMED_MS: u64 = 400;
/// A watchdog retry fires into an idle game (the awaited animation never
/// played): dispatch margin only.
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

/// The ceiling on a single extra wait: one minute.
///
/// The click baselines this adds onto are calibrated to the game's blocking
/// animations and span 100 ms (`scroll_settle`) to 1180 ms (`shop_opened`), and
/// the Setup tab's own meter tops out at 2500 ms total — so 60 000 ms is roughly
/// fifty times the slowest baseline and twenty-four times anything the GUI can
/// produce. Every legitimate "pause like a slow, distracted human" setting stays
/// reachable, plus a wide margin for experimenting past what the UI offers.
///
/// What it makes unreachable is the two ways an unbounded value hurt: a `max_ms`
/// in the tens of minutes silently freezes the refresh loop between two clicks
/// with nothing to distinguish it from a hang, and a value near `u64::MAX`
/// overflows the plain `baseline + extra` sums the timing editor does while
/// painting a range (panic in debug, silent wrap in release).
///
/// It lives here, next to [`DelayRange`], rather than in `config` where it was
/// first written: the type carries the bound now, so the constant belongs where
/// the check is, and the loader is no longer the only place that could apply it.
pub const MAX_TIMING_MS: u64 = 60_000;

/// Why a `(min_ms, max_ms)` pair is not a [`DelayRange`].
///
/// Both messages say what the value *would have done*, because this is the text
/// a player reads in an error window over a file they are told not to hand-edit.
/// Neither names the key: the pair reaches this type either from `config.toml`
/// through `toml`, which prefixes the failing key's line and span, or from a
/// struct literal, where the compiler names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelayRangeError {
    /// `min_ms > max_ms`. With the inline TOML form this table uses
    /// (`{ min_ms = 800, max_ms = 200 }`) swapping the two is an ordinary typo,
    /// and read leniently it becomes a fixed 800 ms delay — the player
    /// configures variability and silently gets none, while the Setup tab shows
    /// "Custom" with no clue why.
    Reversed { min_ms: u64, max_ms: u64 },
    /// `max_ms` past [`MAX_TIMING_MS`]. This is what freezes the loop for ten
    /// minutes between two clicks, and what overflowed the editor's
    /// `baseline + max` sums near `u64::MAX`.
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

/// The wire shape of a [`DelayRange`] — the two keys as `config.toml` spells
/// them, carrying no invariant. It exists only as the `#[serde(try_from)]` hook:
/// deriving `Deserialize` on the newtype itself would let serde fill the private
/// fields directly and skip the check, which is the whole defect this pair fixes.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawDelayRange {
    min_ms: u64,
    max_ms: u64,
}

impl TryFrom<RawDelayRange> for DelayRange {
    type Error = DelayRangeError;

    fn try_from(raw: RawDelayRange) -> Result<Self, Self::Error> {
        DelayRange::try_new(raw.min_ms, raw.max_ms)
    }
}

/// An inclusive extra-wait range, in milliseconds. Each resolved wait draws a
/// uniform value in `[min_ms, max_ms]` and adds it to a tuned baseline, so the
/// loop's pauses vary like a human's instead of being byte-identical every
/// time. The default (`0..=0`) reproduces the calibrated timing exactly; the
/// baseline is the floor, so a range only ever slows the loop down.
///
/// `min_ms <= max_ms <= MAX_TIMING_MS` holds **by construction**: the fields are
/// private and the three ways in ([`try_new`](Self::try_new),
/// [`ceiling`](Self::ceiling), [`set_max_ms`](Self::set_max_ms)) each enforce it,
/// with `Deserialize` routed through `RawDelayRange` so `config.toml` is no
/// exception. Do not move the check back into a loop in `config::validate_timings`
/// and let each producer (a preset, a GUI drag, `persist::save`) re-derive or
/// bypass it — a GUI write one missing clamp away from invalid is a file the
/// next launch refuses, recoverable only by hand-editing a file the app owns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "RawDelayRange")]
pub struct DelayRange {
    min_ms: u64,
    max_ms: u64,
}

impl DelayRange {
    /// The range `min_ms..=max_ms`, or why it is not one.
    ///
    /// # Errors
    ///
    /// [`DelayRangeError::Reversed`] when `min_ms > max_ms`, and
    /// [`DelayRangeError::AboveCeiling`] when `max_ms` is past
    /// [`MAX_TIMING_MS`]. The order matters for the message a player sees: a
    /// reversed pair is reported as reversed even when it also breaks the
    /// ceiling, because swapping it is the fix.
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
    /// Infallible, and that is the point: this is the shape both producers
    /// inside the app make (a preset dials the random ceiling, the Setup tab's
    /// drag sets it), so neither needs a `Result` it could only `expect` on.
    /// `min_ms = 0` cannot reverse the range, and the clamp answers the ceiling.
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

    /// The ceiling of the draw, at most [`MAX_TIMING_MS`], never below
    /// [`min_ms`](Self::min_ms).
    #[must_use]
    pub const fn max_ms(self) -> u64 {
        self.max_ms
    }

    /// Move the ceiling to what the player just dragged to, keeping the
    /// invariant: the value is clamped to [`MAX_TIMING_MS`], and a
    /// config-seeded floor above it comes down with it (min never exceeds the
    /// max the player just set).
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

    /// The inert default (`0..=0`): the calibrated baseline, no extra wait.
    /// Persistence skips these so a first Apply does not fill
    /// `[actuator.timings]` with eight no-op ranges the player never set.
    pub fn is_inert(&self) -> bool {
        self.min_ms == 0 && self.max_ms == 0
    }

    /// A uniform draw in `[min_ms, max_ms]`.
    ///
    /// Plain arithmetic, where the unvalidated version needed a `saturating_sub`
    /// and a `checked_add`: the type's invariant makes every step provable. The
    /// span cannot underflow (`min_ms <= max_ms`); the inclusive `span + 1`
    /// modulus cannot overflow, which is what used to make `% 0` reachable from
    /// a `max_ms = u64::MAX` config file; and the result is at most `max_ms`,
    /// hence at most `MAX_TIMING_MS`.
    fn draw(&self, jitter: &mut Jitter) -> u64 {
        let span = self.max_ms - self.min_ms;
        if span == 0 {
            return self.min_ms;
        }
        self.min_ms + jitter.next() % (span + 1)
    }
}

/// Player-set extra-wait ranges, added on top of every tuned baseline above.
/// All-default (`0..=0`) reproduces the calibrated timing exactly.
///
/// Serialization skips every inert range: `config::persist` replaces the whole
/// `[actuator.timings]` table on each Apply, and writing eight
/// `{ min_ms = 0, max_ms = 0 }` lines the player never asked for would fight
/// that module's whole purpose (preserving the shape of a hand-authored file).
/// The container `#[serde(default)]` makes the omission round-trip exactly.
/// Only whole ranges are skipped, never a single `min_ms = 0` inside a range
/// that *is* written: there the zero is the draw's floor, and the readable
/// `{ min_ms = .., max_ms = .. }` pair is the style the example documents.
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

/// Eight 16-byte ranges. `Copy` is kept deliberately above the usual 64-byte
/// guidance — the type has no heap data, jobs are built a few times per refresh
/// rather than per packet, and every alternative forces a `.clone()` that would
/// signal a cost there is none of. The canary is here so a ninth action is a
/// decision rather than a surprise, in the style of `capture`'s and `stream`'s.
const _: () = assert!(size_of::<Timings>() == 128);

impl Timings {
    /// Every range paired with its `[actuator.timings]` key, in declaration
    /// order — what `Config::validate` walks to bound the player's values.
    ///
    /// The destructuring is exhaustive on purpose: a ninth action added above
    /// stops compiling here until it is named, so validation can never
    /// silently skip a knob that reaches the refresh loop. It destructures
    /// *through the reference* — same exhaustiveness guarantee, without copying
    /// all 128 bytes only to copy eight 16-byte ranges back out of them.
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

    // The five resolved waits below are `pub(super)` for the job builders in
    // `jobs`, which are their only callers: a resolved wait is meaningless
    // without the step it precedes.

    /// The pre-wait for a trigger: its tuned baseline plus a fresh draw from
    /// the matching range.
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

/// One-touch humanization level: a named `Timings` the Setup UI offers before
/// the per-action fine-tuning. Each preset only dials the *random extra* every
/// action can add on top of its tuned baseline (the `max_ms`); `min_ms` stays a
/// config-only floor, so a preset never rewrites the calibrated minimum. Higher
/// levels add more random slack so the loop clicks less like a metronome.
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
    /// The three presets in display order (fastest to most cautious).
    pub const ALL: [TimingPreset; 3] = [
        TimingPreset::Instant,
        TimingPreset::Human,
        TimingPreset::Cautious,
    ];

    /// The player-facing name of the level.
    pub fn label(self) -> &'static str {
        match self {
            TimingPreset::Instant => "Instant",
            TimingPreset::Human => "Human",
            TimingPreset::Cautious => "Cautious",
        }
    }

    /// The `Timings` this level resolves to. Only `max_ms` is set (the random
    /// ceiling); `min_ms` stays 0 so the floor remains a config-only concern.
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

    /// The preset `timings` exactly matches, or `None` when the player has
    /// fine-tuned away from every level ("Custom").
    pub fn from_timings(timings: &Timings) -> Option<TimingPreset> {
        TimingPreset::ALL
            .into_iter()
            .find(|preset| preset.timings() == *timings)
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
        // A preset must never write a floor: `min_ms` stays 0 on every action so
        // the config-only minimum is left untouched.
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
    fn trigger_pre_waits_cover_each_animation() {
        assert_eq!(Trigger::ShopOpened.pre_wait_ms(), 1_180);
        assert_eq!(Trigger::Refreshed.pre_wait_ms(), 780);
        assert_eq!(Trigger::PurchaseResumed.pre_wait_ms(), 400);
        // Recovery fires into an idle game: dispatch margin only.
        assert_eq!(Trigger::Recovery.pre_wait_ms(), 400);
    }

    #[test]
    fn a_reversed_range_cannot_be_built_at_all() {
        // A GUI edit, a future preset and `config.toml` all go through
        // `try_new`, so there is no path left that could read a reversed pair
        // leniently as a fixed delay. The message still says what the value
        // would have been read as, because that tells the player it was a typo.
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
    fn a_range_past_the_ceiling_cannot_be_built_and_the_ceiling_itself_can() {
        // `u64::MAX` would overflow `draw`'s modulus and the editor's
        // `baseline + max` sums; 600_000 (ten minutes) would freeze the loop
        // between two clicks. Both are unrepresentable, and the inclusive
        // bound stays usable — the ceiling exists to stop a frozen loop, not
        // to narrow the knob.
        assert_eq!(
            DelayRange::try_new(0, u64::MAX),
            Err(DelayRangeError::AboveCeiling { max_ms: u64::MAX })
        );
        assert_eq!(
            DelayRange::try_new(0, 600_000),
            Err(DelayRangeError::AboveCeiling { max_ms: 600_000 })
        );
        assert_eq!(range(0, MAX_TIMING_MS).max_ms(), MAX_TIMING_MS);
        // `ceiling` is the infallible door, so it clamps instead of failing.
        assert_eq!(DelayRange::ceiling(u64::MAX).max_ms(), MAX_TIMING_MS);
        assert_eq!(DelayRange::ceiling(0), DelayRange::default());
    }

    #[test]
    fn set_max_ms_keeps_the_invariant_it_could_break() {
        // The Setup tab's drag is the one mutating producer. Dragging below a
        // config-seeded floor must bring the floor down, not leave a reversed
        // range behind — the check `timing_meter` used to make by hand, one line
        // after writing `max_ms` and one line before anything could observe it.
        let mut r = range(400, 900);
        r.set_max_ms(100);
        assert_eq!((r.min_ms(), r.max_ms()), (100, 100));
        r.set_max_ms(u64::MAX);
        assert_eq!((r.min_ms(), r.max_ms()), (100, MAX_TIMING_MS));
    }

    #[test]
    fn only_the_all_zero_range_is_inert() {
        // Drives the `skip_serializing_if` on every `Timings` field: a range
        // wrongly reported inert would be dropped from a saved config.toml,
        // silently reverting the player's setting on the next launch. The old
        // `(1, 0)` case is gone — the type no longer has that value.
        assert!(DelayRange::default().is_inert());
        assert!(range(0, 0).is_inert());
        assert!(!range(0, 1).is_inert());
        assert!(!range(1, 1).is_inert());
    }

    #[test]
    fn named_ranges_covers_every_field_under_its_config_key() {
        // Give each field a distinct value so a copy-paste in the pairing
        // (two keys reading the same field) cannot pass.
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
