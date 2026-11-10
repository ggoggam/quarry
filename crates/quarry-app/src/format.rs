//! Numbers, durations and times as the status bar, the Output view and the
//! query history show them.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local, TimeZone as _};

/// `1234567` as `1,234,567`.
pub fn format_count(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `1 row`, `1,000 rows`.
pub fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{} {word}s", format_count(n))
    }
}

/// `<1 ms`, `42 ms`, `1.50 s`, `1m 15s`.
///
/// Milliseconds are whole ones counted, not rounded, so a run the history
/// kept as `0` ms and the same run timed in full both read `<1 ms`.
pub fn format_elapsed(elapsed: Duration) -> String {
    let ms = elapsed.as_millis();
    if ms < 1 {
        "<1 ms".to_owned()
    } else if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.2} s", elapsed.as_secs_f64())
    } else {
        let secs = elapsed.as_secs();
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

/// Now, in milliseconds since the Unix epoch, as history entries keep time.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn local(ms: i64) -> Option<DateTime<Local>> {
    Local.timestamp_millis_opt(ms).single()
}

/// The local time of day: `18:02:11`.
pub fn clock(ms: i64) -> String {
    local(ms).map_or_else(String::new, |t| t.format("%H:%M:%S").to_string())
}

/// When something ran, relative to `now` where that reads better:
/// `14:02`, `Yesterday 14:02`, `Mon 14:02` within a week, `2026-03-01 14:02`.
pub fn when(ms: i64, now: i64) -> String {
    let (Some(then), Some(now)) = (local(ms), local(now)) else {
        return String::new();
    };
    let days = (now.date_naive() - then.date_naive()).num_days();
    let time = then.format("%H:%M");
    match days {
        0 => time.to_string(),
        1 => format!("Yesterday {time}"),
        2..=6 => format!("{} {time}", then.format("%a")),
        _ => then.format("%Y-%m-%d %H:%M").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_counts_and_durations() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1000), "1,000");
        assert_eq!(format_count(1234567), "1,234,567");
        assert_eq!(plural(1, "row"), "1 row");
        assert_eq!(plural(0, "row"), "0 rows");
        assert_eq!(plural(2048, "row"), "2,048 rows");
        assert_eq!(format_elapsed(Duration::ZERO), "<1 ms");
        assert_eq!(format_elapsed(Duration::from_micros(400)), "<1 ms");
        assert_eq!(format_elapsed(Duration::from_micros(999)), "<1 ms");
        assert_eq!(format_elapsed(Duration::from_micros(1500)), "1 ms");
        assert_eq!(format_elapsed(Duration::from_millis(42)), "42 ms");
        assert_eq!(format_elapsed(Duration::from_micros(999_900)), "999 ms");
        assert_eq!(format_elapsed(Duration::from_millis(1500)), "1.50 s");
        assert_eq!(format_elapsed(Duration::from_secs(75)), "1m 15s");
    }

    #[test]
    fn says_when_relative_to_now() {
        let at = |day, hour, minute, second| {
            Local
                .with_ymd_and_hms(2026, 11, day, hour, minute, second)
                .single()
                .unwrap()
                .timestamp_millis()
        };
        let now = at(10, 12, 0, 0);
        assert_eq!(when(at(10, 10, 0, 0), now), "10:00");
        assert_eq!(when(at(9, 23, 59, 0), now), "Yesterday 23:59");
        assert_eq!(when(at(7, 12, 0, 0), now), "Sat 12:00");
        assert_eq!(when(at(1, 8, 5, 0), now), "2026-11-01 08:05");
        assert_eq!(clock(at(10, 12, 1, 1)), "12:01:01");
    }
}
