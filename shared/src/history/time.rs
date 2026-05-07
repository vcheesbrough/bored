//! Local-timezone date formatting for audit history rows.
//!
//! Inputs are explicit so the helpers stay testable as plain native Rust:
//!
//! - `now_ms` / `then_ms`: milliseconds since the Unix epoch.
//! - `local_offset_minutes`: the user's local offset east of UTC, e.g.
//!   `60` for BST, `-480` for PST. Equivalent to `-Date.getTimezoneOffset()`
//!   in JavaScript (note the sign flip — JS returns offsets the wrong way).
//!
//! Output strings (matches card #132 §2):
//!
//! | Branch                          | Format                          |
//! |---------------------------------|---------------------------------|
//! | < 60 s                          | `Just now`                      |
//! | 1..59 minutes                   | `{N} minute(s) ago`             |
//! | Same local calendar day         | `Today, h:mm a.m./p.m.`         |
//! | Previous local calendar day     | `Yesterday, h:mm a.m./p.m.`     |
//! | 2..6 local days ago             | `Mon h:mm a.m.`                 |
//! | Older, same local year          | `12 Apr h:mm a.m.`              |
//! | Older, different local year     | `1 Dec 2025 h:mm p.m.`          |
//!
//! [`format_history_tooltip`] returns the absolute datetime including a
//! caller-supplied tz label (e.g. `"BST"`); the tz suffix is sourced
//! from `Intl.DateTimeFormat` at call sites because the abbreviation
//! isn't computable from offset alone.

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
// 1970-01-01 was a Thursday → day 0 maps to weekday 4 with this Sun=0 convention.
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// Strip the Surreal `d'…'` wrapper if present; otherwise return the input unchanged.
///
/// Surreal sends `created_at` over the wire as e.g.
/// `"d'2026-05-07T01:27:04.823026281Z'"`. JS `Date.parse` rejects that
/// wrapper, so the frontend strips it before calling `Date::new`.
pub fn strip_surreal_wrapper(raw: &str) -> &str {
    let raw = raw.trim();
    raw.strip_prefix("d'")
        .and_then(|s| s.strip_suffix('\''))
        .unwrap_or(raw)
}

/// Format the relative / "Today / Yesterday / weekday / older" string.
pub fn format_history_time(now_ms: i64, then_ms: i64, local_offset_minutes: i32) -> String {
    let delta = now_ms - then_ms;

    if delta.abs() < MINUTE_MS {
        return "Just now".to_string();
    }
    if (MINUTE_MS..HOUR_MS).contains(&delta) {
        let minutes = delta / MINUTE_MS;
        let plural = if minutes == 1 { "" } else { "s" };
        return format!("{minutes} minute{plural} ago");
    }

    let now_day = local_day_index(now_ms, local_offset_minutes);
    let then_day = local_day_index(then_ms, local_offset_minutes);
    let day_diff = now_day - then_day;

    let (h12, mins, ampm) = local_h12_mm_ampm(then_ms, local_offset_minutes);

    if day_diff == 0 {
        return format!("Today, {h12}:{mins:02} {ampm}");
    }
    if day_diff == 1 {
        return format!("Yesterday, {h12}:{mins:02} {ampm}");
    }
    if (2..7).contains(&day_diff) {
        let wk = WEEKDAYS[local_weekday_index(then_day) as usize];
        return format!("{wk} {h12}:{mins:02} {ampm}");
    }

    let (then_y, then_m, then_d) = civil_from_days(then_day);
    let (now_y, _, _) = civil_from_days(now_day);
    let m = MONTHS[(then_m - 1) as usize];
    if then_y == now_y {
        format!("{then_d} {m} {h12}:{mins:02} {ampm}")
    } else {
        format!("{then_d} {m} {then_y} {h12}:{mins:02} {ampm}")
    }
}

/// Format an absolute tooltip datetime in the user's local timezone with
/// an explicit tz suffix (e.g. `"2026-05-07 17:35 BST"`).
///
/// Pass `tz_label = ""` to omit the suffix (e.g. when `Intl.DateTimeFormat`
/// is unavailable in the host JS engine). The displayed time is always in
/// the user's local tz regardless of `tz_label`.
pub fn format_history_tooltip(then_ms: i64, local_offset_minutes: i32, tz_label: &str) -> String {
    let day = local_day_index(then_ms, local_offset_minutes);
    let (y, m, d) = civil_from_days(day);
    let local_ms = then_ms + (local_offset_minutes as i64) * MINUTE_MS;
    let in_day_ms = local_ms.rem_euclid(DAY_MS);
    let h = (in_day_ms / HOUR_MS) as u32;
    let mm = ((in_day_ms / MINUTE_MS) % 60) as u32;
    if tz_label.is_empty() {
        format!("{y:04}-{m:02}-{d:02} {h:02}:{mm:02}")
    } else {
        format!("{y:04}-{m:02}-{d:02} {h:02}:{mm:02} {tz_label}")
    }
}

fn local_day_index(unix_ms: i64, offset_minutes: i32) -> i64 {
    let local_ms = unix_ms + (offset_minutes as i64) * MINUTE_MS;
    local_ms.div_euclid(DAY_MS)
}

fn local_h12_mm_ampm(unix_ms: i64, offset_minutes: i32) -> (u32, u32, &'static str) {
    let local_ms = unix_ms + (offset_minutes as i64) * MINUTE_MS;
    let in_day_ms = local_ms.rem_euclid(DAY_MS);
    let h24 = (in_day_ms / HOUR_MS) as u32;
    let mins = ((in_day_ms / MINUTE_MS) % 60) as u32;
    let (h12, ampm) = match h24 {
        0 => (12, "a.m."),
        1..=11 => (h24, "a.m."),
        12 => (12, "p.m."),
        _ => (h24 - 12, "p.m."),
    };
    (h12, mins, ampm)
}

/// Sun=0, Mon=1, …, Sat=6 — matching the [`WEEKDAYS`] table above.
fn local_weekday_index(local_day: i64) -> i32 {
    (local_day + 4).rem_euclid(7) as i32
}

/// Howard Hinnant's `civil_from_days` — pure integer arithmetic, no tz
/// table required. Returns `(year, month [1..=12], day [1..=31])`.
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convert a UTC date to unix millis. Test-only helper so we don't
    /// depend on `chrono` for fixture construction.
    fn ms(y: i32, m: u32, d: u32, h: u32, mm: u32) -> i64 {
        let days = days_from_civil(y, m, d);
        days * DAY_MS + (h as i64) * HOUR_MS + (mm as i64) * MINUTE_MS
    }

    fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
        let y = if m <= 2 { y - 1 } else { y } as i64;
        let era = if y >= 0 { y / 400 } else { (y - 399) / 400 };
        let yoe = (y - era * 400) as u64;
        let m_internal = if m > 2 { m - 3 } else { m + 9 };
        let doy = (153 * m_internal as u64 + 2) / 5 + d as u64 - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe as i64 - 719_468
    }

    #[test]
    fn strip_surreal_wrapper_unwraps() {
        assert_eq!(
            strip_surreal_wrapper("d'2026-05-07T01:27:04.823Z'"),
            "2026-05-07T01:27:04.823Z"
        );
        assert_eq!(
            strip_surreal_wrapper("2026-05-07T01:27:04.823Z"),
            "2026-05-07T01:27:04.823Z"
        );
    }

    #[test]
    fn just_now_under_a_minute() {
        let now = ms(2026, 5, 7, 12, 30);
        let then = now - 30_000;
        assert_eq!(format_history_time(now, then, 0), "Just now");
    }

    #[test]
    fn n_minutes_ago_within_an_hour() {
        let now = ms(2026, 5, 7, 12, 30);
        assert_eq!(
            format_history_time(now, now - 5 * MINUTE_MS, 0),
            "5 minutes ago"
        );
        assert_eq!(format_history_time(now, now - MINUTE_MS, 0), "1 minute ago");
        assert_eq!(
            format_history_time(now, now - 59 * MINUTE_MS, 0),
            "59 minutes ago"
        );
    }

    #[test]
    fn today_uses_local_clock_time() {
        // Event 14:30 UTC = 15:30 BST, "now" 22:00 UTC = 23:00 BST same local day.
        let now = ms(2026, 5, 7, 22, 0);
        let then = ms(2026, 5, 7, 14, 30);
        assert_eq!(format_history_time(now, then, 60), "Today, 3:30 p.m.");
    }

    #[test]
    fn yesterday_uses_local_clock_time() {
        let now = ms(2026, 5, 8, 6, 0);
        let then = ms(2026, 5, 7, 11, 5);
        assert_eq!(format_history_time(now, then, 0), "Yesterday, 11:05 a.m.");
    }

    #[test]
    fn within_a_week_shows_weekday() {
        // 2026-05-07 = Thursday; now is Mon 2026-05-11.
        let now = ms(2026, 5, 11, 9, 0);
        let then = ms(2026, 5, 7, 14, 30);
        assert_eq!(format_history_time(now, then, 0), "Thu 2:30 p.m.");
    }

    #[test]
    fn older_uses_short_date_no_year_when_same_year() {
        let now = ms(2026, 5, 30, 9, 0);
        let then = ms(2026, 4, 12, 9, 30);
        assert_eq!(format_history_time(now, then, 0), "12 Apr 9:30 a.m.");
    }

    #[test]
    fn older_includes_year_when_different_year() {
        let now = ms(2026, 1, 5, 9, 0);
        let then = ms(2025, 12, 1, 18, 7);
        assert_eq!(format_history_time(now, then, 0), "1 Dec 2025 6:07 p.m.");
    }

    /// Same UTC moments, but different local offsets → different headlines.
    /// This is the timezone-boundary case from card #132 §2.
    #[test]
    fn timezone_boundary_changes_today_or_yesterday() {
        // Event 02:00 UTC on Thu, "now" 14:00 UTC on Thu (12 h later).
        let event = ms(2026, 5, 7, 2, 0);
        let now = ms(2026, 5, 7, 14, 0);

        // BST (UTC+1): event = 03:00 Thu, now = 15:00 Thu → same local day.
        assert_eq!(format_history_time(now, event, 60), "Today, 3:00 a.m.");

        // PST (UTC-8): event = 18:00 Wed, now = 06:00 Thu → previous local day.
        assert_eq!(
            format_history_time(now, event, -480),
            "Yesterday, 6:00 p.m."
        );
    }

    #[test]
    fn midnight_renders_as_12_am_and_noon_as_12_pm() {
        let now_evening = ms(2026, 5, 7, 18, 0);
        assert_eq!(
            format_history_time(now_evening, ms(2026, 5, 7, 0, 0), 0),
            "Today, 12:00 a.m."
        );
        assert_eq!(
            format_history_time(now_evening, ms(2026, 5, 7, 12, 0), 0),
            "Today, 12:00 p.m."
        );
    }

    #[test]
    fn future_event_within_a_minute_still_just_now() {
        // Clock skew can produce slightly-future timestamps; treat them as
        // "Just now" rather than producing a negative-minute string.
        let now = ms(2026, 5, 7, 12, 0);
        let then = now + 30_000;
        assert_eq!(format_history_time(now, then, 0), "Just now");
    }

    #[test]
    fn tooltip_includes_tz_suffix_when_supplied() {
        let then = ms(2026, 5, 7, 16, 35);
        assert_eq!(
            format_history_tooltip(then, 60, "BST"),
            "2026-05-07 17:35 BST"
        );
        assert_eq!(
            format_history_tooltip(then, 0, "UTC"),
            "2026-05-07 16:35 UTC"
        );
    }

    #[test]
    fn tooltip_omits_tz_when_label_empty() {
        let then = ms(2026, 5, 7, 16, 35);
        assert_eq!(format_history_tooltip(then, 0, ""), "2026-05-07 16:35");
    }

    #[test]
    fn tooltip_in_negative_offset_zone() {
        // PST: 16:35 UTC → 08:35 local previous? No — 16:35 UTC - 8h = 08:35
        // same day local.
        let then = ms(2026, 5, 7, 16, 35);
        assert_eq!(
            format_history_tooltip(then, -480, "PDT"),
            "2026-05-07 08:35 PDT"
        );
    }
}
