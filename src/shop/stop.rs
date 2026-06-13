use std::time::Duration;

use crate::detector::alias;

pub const SHOP_SLOTS_PER_REFRESH: u32 = 6;

pub const MYSTIC_DROP_PER_SLOT: f64 = 0.001_700_646;
pub const COVENANT_DROP_PER_SLOT: f64 = 0.006_602_509;

pub mod prices {
    pub const MYSTIC_MEDAL: u32 = 280_000;
    pub const COVENANT_BOOKMARK: u32 = 185_000;
}

pub(crate) fn gold_spent_for(bought: impl Fn(&str) -> u32) -> u64 {
    u64::from(bought(alias::MYSTIC_MEDAL)) * u64::from(prices::MYSTIC_MEDAL)
        + u64::from(bought(alias::COVENANT)) * u64::from(prices::COVENANT_BOOKMARK)
}

/// First condition in fixed priority order
/// (duration → mystic → covenant → gold) so the reason is deterministic.
pub(crate) fn stop_condition_for(
    shop: &crate::config::ShopConfig,
    elapsed: Duration,
    bought: impl Fn(&str) -> u32,
) -> Option<&'static str> {
    if shop.stop_after_minutes > 0
        && elapsed >= Duration::from_secs(u64::from(shop.stop_after_minutes) * 60)
    {
        return Some("stop_after_minutes");
    }
    if shop.stop_when_mystic_medals > 0
        && bought(alias::MYSTIC_MEDAL) >= shop.stop_when_mystic_medals
    {
        return Some("stop_when_mystic_medals");
    }
    if shop.stop_when_covenants > 0 && bought(alias::COVENANT) >= shop.stop_when_covenants {
        return Some("stop_when_covenants");
    }
    if shop.stop_when_gold_spent > 0
        && gold_spent_for(&bought) >= u64::from(shop.stop_when_gold_spent)
    {
        return Some("stop_when_gold_spent");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ShopConfig;

    fn shop_with(max_refreshes: u32, minutes: u32, mystic: u32, covenants: u32) -> ShopConfig {
        ShopConfig {
            max_refreshes,
            buy_mystic_medals: true,
            buy_covenant: true,
            max_scrolls_per_round: 3,
            buy_button_y_offset_ratio: 0.04,
            buy_button_band_h_ratio: 0.04,
            buy_calibration_line_y_ratio: 0.55,
            stop_after_minutes: minutes,
            stop_when_mystic_medals: mystic,
            stop_when_covenants: covenants,
            stop_when_gold_spent: 0,
            sleep_when_done: false,
        }
    }

    #[test]
    fn stop_condition_none_when_all_zero() {
        let cfg = shop_with(0, 0, 0, 0);
        let reason = stop_condition_for(&cfg, Duration::from_secs(3600), |_| 999);
        assert_eq!(reason, None);
    }

    #[test]
    fn stop_condition_fires_on_minutes() {
        let cfg = shop_with(0, 5, 0, 0);
        let reason = stop_condition_for(&cfg, Duration::from_secs(300), |_| 0);
        assert_eq!(reason, Some("stop_after_minutes"));
        let reason = stop_condition_for(&cfg, Duration::from_secs(299), |_| 0);
        assert_eq!(reason, None);
    }

    #[test]
    fn stop_condition_fires_on_per_alias_counts() {
        let cfg = shop_with(0, 0, 3, 0);
        let count = |a: &str| {
            if a == alias::MYSTIC_MEDAL { 3 } else { 0 }
        };
        assert_eq!(
            stop_condition_for(&cfg, Duration::from_secs(0), count),
            Some("stop_when_mystic_medals")
        );
    }

    #[test]
    fn stop_condition_priority_is_duration_then_mystic_then_covenant() {
        let cfg = shop_with(0, 1, 5, 5);
        let reason = stop_condition_for(&cfg, Duration::from_secs(60), |_| 10);
        assert_eq!(reason, Some("stop_after_minutes"));
        let cfg = shop_with(0, 0, 5, 5);
        let reason = stop_condition_for(&cfg, Duration::from_secs(0), |_| 10);
        assert_eq!(reason, Some("stop_when_mystic_medals"));
        let cfg = shop_with(0, 0, 0, 5);
        let reason = stop_condition_for(&cfg, Duration::from_secs(0), |_| 10);
        assert_eq!(reason, Some("stop_when_covenants"));
    }

    #[test]
    fn stop_condition_ignores_count_when_threshold_is_zero() {
        let cfg = shop_with(0, 0, 0, 0);
        let reason = stop_condition_for(&cfg, Duration::from_secs(0), |_| 100);
        assert_eq!(reason, None);
    }

    #[test]
    fn stop_condition_fires_when_gold_spent_target_reached() {
        let mut cfg = shop_with(0, 0, 0, 0);
        cfg.stop_when_gold_spent = 1_000_000;
        // 3 mystic × 280k + 1 covenant × 185k = 1_025_000 ≥ 1_000_000.
        let count = |a: &str| match a {
            "mystic_medal" => 3,
            "covenant" => 1,
            _ => 0,
        };
        assert_eq!(
            stop_condition_for(&cfg, Duration::from_secs(0), count),
            Some("stop_when_gold_spent")
        );
    }

    #[test]
    fn stop_condition_gold_does_not_fire_below_threshold() {
        let mut cfg = shop_with(0, 0, 0, 0);
        cfg.stop_when_gold_spent = 1_000_000;
        let count = |a: &str| if a == "mystic_medal" { 3 } else { 0 }; // 840k
        assert_eq!(
            stop_condition_for(&cfg, Duration::from_secs(0), count),
            None
        );
    }

    #[test]
    fn stop_condition_fires_at_exact_threshold_not_below() {
        let cfg = shop_with(0, 0, 5, 0);
        assert_eq!(
            stop_condition_for(&cfg, Duration::from_secs(0), |_| 5),
            Some("stop_when_mystic_medals")
        );
        assert_eq!(
            stop_condition_for(&cfg, Duration::from_secs(0), |_| 4),
            None
        );
    }
}
