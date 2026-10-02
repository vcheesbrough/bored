//! The one place a stored instant becomes the string the public API returns
//! (card #472).
//!
//! # The wire format
//!
//! RFC 3339, always UTC with a `Z` suffix, always exactly six fractional
//! digits: `2026-10-02T16:17:12.643091Z`.
//!
//! - **UTC + `Z`** — one spelling per instant, so clients can compare the
//!   strings as text (the SPA's recency sort does exactly that).
//! - **Fixed microsecond precision** — Postgres `timestamptz` stores
//!   microseconds, so when storage moves there (card #471) it can produce
//!   these exact bytes. A nanosecond format would have forced a second wire
//!   change then. "Fixed" means `.000000Z` is written out, never dropped, so
//!   every timestamp is the same length.
//! - **Truncated, not rounded** — `…12.643091999Z` becomes `…12.643091Z`,
//!   never `…12.643092Z`. A reported time is never later than the stored
//!   one, so it can never appear to come from the future, and rounding can
//!   never carry a value over into the next second, day or year.
//!
//! # Why bored owns this, not the database driver
//!
//! Before #472 every model called `surrealdb::sql::Datetime::to_string()`,
//! which renders a *SurrealQL literal*: `d'2026-10-02T16:17:12.643091493Z'`.
//! That is a query-language token, not a timestamp, and it leaked storage
//! detail into REST bodies, SSE events, MCP output and (via `into_api()`)
//! the audit snapshots. Owning the format here makes it a decision bored
//! controls rather than something a driver upgrade can change underneath
//! the API.

/// Earliest instant RFC 3339 can spell: `0000-01-01T00:00:00Z`, as Unix seconds.
///
/// RFC 3339 years are exactly four digits, so anything earlier has no valid
/// spelling at all. Hand-computed: 1970 years × 365 days + 478 leap days
/// = 719 528 days before the epoch; × 86 400 s.
const MIN_UNIX_SECS: i64 = -62_167_219_200;

/// Latest whole second RFC 3339 can spell: `9999-12-31T23:59:59Z`, as Unix seconds.
const MAX_UNIX_SECS: i64 = 253_402_300_799;

/// The largest microsecond count that still fits in one second.
const MAX_MICROS: u32 = 999_999;

/// Format a SurrealDB datetime for the public API.
///
/// This is what every `into_api()` in `models.rs` calls. It only *reads* the
/// instant out of the driver's type — `Datetime` is a thin wrapper around
/// `chrono::DateTime<Utc>` (the `.0` field) — and hands the two numbers that
/// fully describe it to [`format_utc_micros`], which knows nothing about
/// SurrealDB.
///
/// `timestamp()` and `timestamp_subsec_nanos()` are inherent methods on
/// chrono's `DateTime`, which is why this file can call them without bored
/// depending on `chrono` directly.
pub fn api_timestamp(datetime: &surrealdb::sql::Datetime) -> String {
    format_utc_micros(datetime.0.timestamp(), datetime.0.timestamp_subsec_nanos())
}

/// Format an instant given as Unix seconds plus a sub-second nanosecond count.
///
/// The arguments are the storage-neutral description of an instant, so when
/// card #471 moves storage to Postgres the new row type can call this directly
/// and the output stays byte-identical.
///
/// - `unix_secs` is whole seconds since `1970-01-01T00:00:00Z`; negative is
///   before the epoch. It is the *floor*, so `-0.5 s` is `(-1, 500_000_000)`.
/// - `subsec_nanos` is normally `0..1_000_000_000`. chrono can hand back up
///   to `1_999_999_999` to represent a leap second (`23:59:60`); RFC 3339
///   cannot be relied on to round-trip that, so it is pinned to the last
///   microsecond of the preceding second (`23:59:59.999999`). That keeps the
///   output inside its own grammar and, like truncation, never later than the
///   real instant.
///
/// Instants outside the years 0000–9999 cannot be written as RFC 3339 at all
/// (the year is exactly four digits). The API only ever stores `time::now()`,
/// so this cannot happen in practice; if it ever does, the value saturates to
/// the nearest representable instant rather than emitting a malformed string
/// or panicking a request.
pub fn format_utc_micros(unix_secs: i64, subsec_nanos: u32) -> String {
    // Integer division truncates toward zero, and `subsec_nanos` is never
    // negative, so this always truncates *toward the past* — the "never moves
    // forward in time" rule. `.min` applies the leap-second pin above.
    let micros = (subsec_nanos / 1_000).min(MAX_MICROS);

    // Saturate out-of-range instants (see the doc comment). A tuple lets each
    // branch decide both the seconds and the fraction together: below the
    // range we pin to the very first instant, above it to the very last.
    let (secs, micros) = if unix_secs < MIN_UNIX_SECS {
        (MIN_UNIX_SECS, 0)
    } else if unix_secs > MAX_UNIX_SECS {
        (MAX_UNIX_SECS, MAX_MICROS)
    } else {
        (unix_secs, micros)
    };

    // The `time` crate does the calendar arithmetic (leap years and all).
    // `from_unix_timestamp` only fails outside ±9999 years, and the clamp
    // above already guarantees we are inside 0000–9999, so the `Err` arm is
    // unreachable. It still returns a valid string rather than panicking:
    // a request should never crash over a timestamp.
    let Ok(utc) = time::OffsetDateTime::from_unix_timestamp(secs) else {
        return "0000-01-01T00:00:00.000000Z".to_string();
    };

    // `{:04}` / `{:02}` zero-pad to fixed widths; `{:06}` pads the
    // microseconds so `.5 s` prints as `.500000`, not `.5`.
    // `u8::from(month)` turns the `time::Month` enum into 1..=12.
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
        micros,
    )
}

#[cfg(test)]
mod tests {
    //! Every expected string here is written out by hand (an independent
    //! oracle): none of them is produced by calling the formatter, the `time`
    //! crate, or anything else on the implementation's side. Inputs given as
    //! text are parsed by SurrealDB's own datetime parser, which is a
    //! different code path from the one under test.

    use super::*;

    /// Parse an RFC 3339 string with SurrealDB's parser, then format it.
    fn via_surreal(input: &str) -> String {
        let datetime = surrealdb::sql::Datetime::try_from(input)
            .unwrap_or_else(|()| panic!("test input {input:?} is not a datetime"));
        api_timestamp(&datetime)
    }

    #[test]
    fn nanoseconds_are_truncated_to_microseconds() {
        assert_eq!(
            via_surreal("2026-10-02T16:17:12.643091493Z"),
            "2026-10-02T16:17:12.643091Z"
        );
    }

    #[test]
    fn truncation_never_rounds_up() {
        // .999999999 would round to the next second; truncation keeps it put.
        assert_eq!(
            via_surreal("2026-10-02T16:17:12.643091999Z"),
            "2026-10-02T16:17:12.643091Z"
        );
    }

    #[test]
    fn zero_sub_seconds_are_written_out() {
        assert_eq!(
            via_surreal("2026-01-01T00:00:00Z"),
            "2026-01-01T00:00:00.000000Z"
        );
    }

    #[test]
    fn short_fractions_are_padded() {
        assert_eq!(
            via_surreal("2026-05-07T01:27:04.5Z"),
            "2026-05-07T01:27:04.500000Z"
        );
    }

    #[test]
    fn leap_day_at_the_last_nanosecond_stays_on_the_leap_day() {
        // Rounding would carry this into 1 March; truncation must not.
        assert_eq!(
            via_surreal("2024-02-29T23:59:59.999999999Z"),
            "2024-02-29T23:59:59.999999Z"
        );
    }

    #[test]
    fn a_non_utc_offset_is_converted_to_utc() {
        // 01:30 at +02:00 is 23:30 UTC on the previous day.
        assert_eq!(
            via_surreal("2026-03-01T01:30:00.25+02:00"),
            "2026-02-28T23:30:00.250000Z"
        );
    }

    #[test]
    fn the_unix_epoch() {
        assert_eq!(format_utc_micros(0, 0), "1970-01-01T00:00:00.000000Z");
    }

    #[test]
    fn half_a_second_before_the_epoch() {
        // Seconds are floored, so -0.5 s is (-1 s, +0.5 s).
        assert_eq!(
            format_utc_micros(-1, 500_000_000),
            "1969-12-31T23:59:59.500000Z"
        );
    }

    #[test]
    fn a_leap_second_is_pinned_to_the_preceding_microsecond() {
        // 1 483 228 799 is 2016-12-31T23:59:59Z, the second before the
        // 2016 leap second. chrono represents 23:59:60.5 as nanos 1.5e9.
        assert_eq!(
            format_utc_micros(1_483_228_799, 1_500_000_000),
            "2016-12-31T23:59:59.999999Z"
        );
    }

    #[test]
    fn the_representable_range_edges() {
        assert_eq!(
            format_utc_micros(-62_167_219_200, 0),
            "0000-01-01T00:00:00.000000Z"
        );
        assert_eq!(
            format_utc_micros(253_402_300_799, 999_999_000),
            "9999-12-31T23:59:59.999999Z"
        );
    }

    #[test]
    fn instants_outside_the_range_saturate() {
        assert_eq!(
            format_utc_micros(i64::MIN, 123_456_789),
            "0000-01-01T00:00:00.000000Z"
        );
        assert_eq!(
            format_utc_micros(i64::MAX, 0),
            "9999-12-31T23:59:59.999999Z"
        );
    }
}
