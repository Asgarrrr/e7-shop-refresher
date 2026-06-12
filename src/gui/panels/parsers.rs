pub(in crate::gui) fn format_minutes(total: u32) -> String {
    let hours = total / 60;
    let minutes = total % 60;
    match (hours, minutes) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h{m:02}"),
    }
}

/// Accepts: empty, bare integer (minutes), `30m`, `30 min`, `2h`, `1h30`,
/// `1h 30`, `1h30m`. `None` on garbage so DragValue keeps the previous
/// value rather than silently zeroing on a typo.
pub(in crate::gui) fn parse_minutes(input: &str) -> Option<u32> {
    let s = input.trim().to_ascii_lowercase();
    if s.is_empty() {
        return Some(0);
    }
    if let Some(h_pos) = s.find('h') {
        let hours: u32 = s[..h_pos].trim().parse().ok()?;
        let rest = strip_minute_suffix(s[h_pos + 1..].trim());
        let minutes: u32 = if rest.is_empty() {
            0
        } else {
            rest.parse().ok()?
        };
        return Some(hours * 60 + minutes);
    }
    strip_minute_suffix(&s).parse().ok()
}

fn strip_minute_suffix(s: &str) -> &str {
    let s = s
        .strip_suffix("min")
        .or_else(|| s.strip_suffix('m'))
        .unwrap_or(s);
    s.trim()
}

pub(in crate::gui) fn format_gold(n: u32) -> String {
    if n < 1_000 {
        return format!("{n}g");
    }
    if n < 1_000_000 {
        let k = n / 1_000;
        let tenths = (n % 1_000) / 100;
        return if k < 10 {
            format!("{k}.{tenths}Kg")
        } else {
            format!("{k}Kg")
        };
    }
    let m = n / 1_000_000;
    let tenths = (n % 1_000_000) / 100_000;
    if m < 10 {
        format!("{m}.{tenths}Mg")
    } else {
        format!("{m}Mg")
    }
}

/// Accepts: empty, bare integer, thousands-separated (`1,500`), `K`/`M`
/// magnitude suffixes, optional `g` unit, any case. `None` on garbage.
pub(in crate::gui) fn parse_gold(input: &str) -> Option<u32> {
    let s: String = input
        .trim()
        .trim_end_matches('g')
        .trim_end_matches('G')
        .chars()
        .filter(|c| *c != ',' && *c != ' ' && *c != '_')
        .collect();
    if s.is_empty() {
        return Some(0);
    }
    let (digits, multiplier) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1_000.0_f64),
        Some('M' | 'm') => (&s[..s.len() - 1], 1_000_000.0_f64),
        _ => (s.as_str(), 1.0_f64),
    };
    if multiplier > 1.0 {
        let value: f64 = digits.parse().ok()?;
        if !value.is_finite() || value < 0.0 {
            return None;
        }
        let scaled = (value * multiplier).round();
        if scaled > u32::MAX as f64 {
            return None;
        }
        Some(scaled as u32)
    } else {
        digits.parse().ok()
    }
}

pub(in crate::gui) fn format_ms(ms: u64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let seconds = ms as f64 / 1000.0;
    if seconds < 10.0 {
        format!("{seconds:.1}s")
    } else {
        format!("{}s", seconds.round() as u64)
    }
}

/// Accepts: bare integer (ms), `500ms`, `1.5s`, `12s`, `2 s`. `None` on
/// garbage so DragValue keeps the previous value.
pub(in crate::gui) fn parse_ms(input: &str) -> Option<u64> {
    let s = input.trim().to_ascii_lowercase();
    if s.is_empty() {
        return Some(0);
    }
    if let Some(rest) = s.strip_suffix("ms") {
        return rest.trim().parse().ok();
    }
    if let Some(rest) = s.strip_suffix('s') {
        let v: f64 = rest.trim().parse().ok()?;
        return Some((v * 1000.0).round() as u64);
    }
    s.parse().ok()
}

/// `DragValue` wired with `format_ms` / `parse_ms`, optionally prefixed
/// with "min "/"max " so it can be used inside min/max pairs without
/// extra plumbing per call site.
pub(in crate::gui) fn ms_drag<'a, Num: egui::emath::Numeric>(
    value: &'a mut Num,
    speed: f32,
    range: std::ops::RangeInclusive<Num>,
    prefix: &'static str,
) -> egui::DragValue<'a> {
    egui::DragValue::new(value)
        .speed(speed)
        .range(range)
        .custom_formatter(move |n, _| {
            let s = format_ms(n.max(0.0).round() as u64);
            if prefix.is_empty() {
                s
            } else {
                format!("{prefix}{s}")
            }
        })
        .custom_parser(move |s| {
            let s = if prefix.is_empty() {
                s
            } else {
                s.strip_prefix(prefix).unwrap_or(s)
            };
            parse_ms(s).map(|v| v as f64)
        })
}

#[cfg(test)]
mod tests {
    use super::{format_gold, format_minutes, format_ms, parse_gold, parse_minutes, parse_ms};

    #[test]
    fn format_minutes_handles_canonical_cases() {
        assert_eq!(format_minutes(0), "0m");
        assert_eq!(format_minutes(1), "1m");
        assert_eq!(format_minutes(45), "45m");
        assert_eq!(format_minutes(60), "1h");
        assert_eq!(format_minutes(90), "1h30");
        assert_eq!(format_minutes(125), "2h05");
        assert_eq!(format_minutes(1440), "24h");
    }

    #[test]
    fn parse_minutes_accepts_canonical_forms() {
        assert_eq!(parse_minutes(""), Some(0));
        assert_eq!(parse_minutes("0"), Some(0));
        assert_eq!(parse_minutes("45"), Some(45));
        assert_eq!(parse_minutes("45m"), Some(45));
        assert_eq!(parse_minutes("45min"), Some(45));
        assert_eq!(parse_minutes("2h"), Some(120));
        assert_eq!(parse_minutes("1h30"), Some(90));
        assert_eq!(parse_minutes("1h 30"), Some(90));
        assert_eq!(parse_minutes("1h30m"), Some(90));
        assert_eq!(parse_minutes("1H30"), Some(90));
    }

    #[test]
    fn parse_minutes_rejects_garbage() {
        assert!(parse_minutes("abc").is_none());
        assert!(parse_minutes("1.5h").is_none());
        assert!(parse_minutes("xh30").is_none());
    }

    #[test]
    fn format_gold_uses_k_m_condensers() {
        assert_eq!(format_gold(0), "0g");
        assert_eq!(format_gold(999), "999g");
        assert_eq!(format_gold(1_000), "1.0Kg");
        assert_eq!(format_gold(1_500), "1.5Kg");
        assert_eq!(format_gold(12_345), "12Kg");
        assert_eq!(format_gold(280_000), "280Kg");
        assert_eq!(format_gold(1_000_000), "1.0Mg");
        assert_eq!(format_gold(2_500_000), "2.5Mg");
        assert_eq!(format_gold(100_000_000), "100Mg");
    }

    #[test]
    fn parse_gold_accepts_canonical_forms() {
        assert_eq!(parse_gold(""), Some(0));
        assert_eq!(parse_gold("0"), Some(0));
        assert_eq!(parse_gold("500"), Some(500));
        assert_eq!(parse_gold("500g"), Some(500));
        assert_eq!(parse_gold("1,500"), Some(1500));
        assert_eq!(parse_gold("1.5K"), Some(1500));
        assert_eq!(parse_gold("280K"), Some(280_000));
        assert_eq!(parse_gold("280Kg"), Some(280_000));
        assert_eq!(parse_gold("1.5m"), Some(1_500_000));
    }

    #[test]
    fn parse_gold_rejects_garbage() {
        assert!(parse_gold("abc").is_none());
        assert!(parse_gold("1.5Q").is_none());
        assert!(parse_gold("-100").is_none());
    }

    #[test]
    fn format_ms_picks_ms_or_seconds_by_magnitude() {
        assert_eq!(format_ms(0), "0ms");
        assert_eq!(format_ms(500), "500ms");
        assert_eq!(format_ms(999), "999ms");
        assert_eq!(format_ms(1_000), "1.0s");
        assert_eq!(format_ms(1_500), "1.5s");
        assert_eq!(format_ms(9_900), "9.9s");
        assert_eq!(format_ms(10_000), "10s");
        assert_eq!(format_ms(12_345), "12s");
    }

    #[test]
    fn parse_ms_accepts_canonical_forms() {
        assert_eq!(parse_ms(""), Some(0));
        assert_eq!(parse_ms("500"), Some(500));
        assert_eq!(parse_ms("500ms"), Some(500));
        assert_eq!(parse_ms("1.5s"), Some(1500));
        assert_eq!(parse_ms("12s"), Some(12_000));
        assert_eq!(parse_ms("2 s"), Some(2_000));
    }

    #[test]
    fn parse_ms_rejects_garbage() {
        assert!(parse_ms("abc").is_none());
        assert!(parse_ms("1.5x").is_none());
    }

    #[test]
    fn format_and_parse_round_trip_for_typical_values() {
        for raw in [0u32, 1, 30, 60, 90, 125, 600, 1439] {
            let s = format_minutes(raw);
            assert_eq!(parse_minutes(&s), Some(raw), "minutes round trip for {raw}");
        }
        for raw in [0u32, 100, 999, 1_000, 1_500, 12_345, 280_000, 1_500_000] {
            let s = format_gold(raw);
            let parsed = parse_gold(&s).expect("gold parse");
            // Allow drift up to 10% — K/M format truncates tens of thousands.
            let drift = (parsed as i64 - raw as i64).unsigned_abs() as u32;
            assert!(
                drift <= raw / 10 + 100,
                "gold round trip drifted too far: {raw} → {s} → {parsed}"
            );
        }
        for raw in [0u64, 50, 500, 999, 1_000, 1_500, 9_900, 10_000, 60_000] {
            let s = format_ms(raw);
            let parsed = parse_ms(&s).expect("ms parse");
            let drift = (parsed as i64 - raw as i64).unsigned_abs();
            assert!(
                drift <= raw / 20 + 50,
                "ms round trip drifted too far: {raw} → {s} → {parsed}"
            );
        }
    }
}
