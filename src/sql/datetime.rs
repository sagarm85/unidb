// Temporal parsing/formatting (P2.a): dependency-light conversions between
// the canonical SQL text forms and the integer representations the row
// encoder stores. No `chrono` — the calendar math is a few lines (Howard
// Hinnant's `days_from_civil`/`civil_from_days`), which keeps the on-disk
// story dependency-light and gives us exact byte control, in the same spirit
// as the hand-rolled row encoding (CLAUDE.md §4).
//
// Representations (all little-endian on disk; see `sql/executor.rs`):
//   TIMESTAMP -> i64 microseconds since the Unix epoch (1970-01-01T00:00:00Z),
//                UTC. No zone is tracked; `TIMESTAMPTZ` normalizes to UTC on
//                input (v1 does not accept an explicit offset yet).
//
// DATE/TIME land in P2.b and will reuse `days_from_civil`/`civil_from_days`
// and the time-of-day split below.

use crate::error::{DbError, Result};

const MICROS_PER_SEC: i64 = 1_000_000;
const SECS_PER_DAY: i64 = 86_400;

/// Days since 1970-01-01 for the given proleptic-Gregorian civil date.
/// Valid for any `y`; `m` in `1..=12`, `d` in `1..=31`. (Hinnant's algorithm.)
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Inverse of [`days_from_civil`]: `(year, month, day)` for a day count since
/// the Unix epoch.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        2 => 28,
        _ => 0,
    }
}

fn parse_uint(s: &str) -> Result<i64> {
    s.parse::<i64>()
        .map_err(|_| DbError::SqlPlan(format!("invalid timestamp field: {s:?}")))
}

/// Parse `'YYYY-MM-DD'`, `'YYYY-MM-DD HH:MM:SS'`, `'YYYY-MM-DDTHH:MM:SS'`, or
/// either of the latter with a fractional-second suffix `.f` up to 6 digits
/// (`.123456`). An optional trailing `Z` is accepted (already UTC). Returns
/// microseconds since the Unix epoch, UTC.
pub fn parse_timestamp(input: &str) -> Result<i64> {
    let s = input.trim();
    let s = s.strip_suffix('Z').unwrap_or(s).trim_end();
    // Split date and (optional) time on the first space or 'T'.
    let (date_part, time_part) = match s.find(['T', ' ']) {
        Some(i) => (&s[..i], s[i + 1..].trim()),
        None => (s, ""),
    };

    let mut dparts = date_part.split('-');
    let (y, mo, d) = match (dparts.next(), dparts.next(), dparts.next(), dparts.next()) {
        (Some(y), Some(mo), Some(d), None) => (parse_uint(y)?, parse_uint(mo)?, parse_uint(d)?),
        _ => {
            return Err(DbError::SqlPlan(format!(
                "invalid timestamp {input:?}: expected 'YYYY-MM-DD[ HH:MM:SS[.ffffff]]'"
            )))
        }
    };
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) {
        return Err(DbError::SqlPlan(format!(
            "invalid timestamp {input:?}: {y:04}-{mo:02}-{d:02} is not a real date"
        )));
    }

    let (hh, mm, ss, micros) = if time_part.is_empty() {
        (0, 0, 0, 0)
    } else {
        let (hms, frac) = match time_part.split_once('.') {
            Some((hms, frac)) => (hms, frac),
            None => (time_part, ""),
        };
        let mut tparts = hms.split(':');
        let (hh, mm, ss) = match (tparts.next(), tparts.next(), tparts.next(), tparts.next()) {
            (Some(h), Some(m), Some(s), None) => (parse_uint(h)?, parse_uint(m)?, parse_uint(s)?),
            (Some(h), Some(m), None, None) => (parse_uint(h)?, parse_uint(m)?, 0),
            _ => {
                return Err(DbError::SqlPlan(format!(
                    "invalid time-of-day in timestamp {input:?}"
                )))
            }
        };
        if hh > 23 || mm > 59 || ss > 59 {
            return Err(DbError::SqlPlan(format!(
                "invalid time-of-day in timestamp {input:?}"
            )));
        }
        let micros = parse_fraction(frac, input)?;
        (hh, mm, ss, micros)
    };

    let days = days_from_civil(y, mo, d);
    let secs = days * SECS_PER_DAY + hh * 3600 + mm * 60 + ss;
    Ok(secs * MICROS_PER_SEC + micros)
}

/// A fractional-second string like `"5"` (=> 500000 µs) or `"123456"`. More
/// than 6 digits is rejected rather than silently truncated (exactness, same
/// discipline as DECIMAL).
fn parse_fraction(frac: &str, input: &str) -> Result<i64> {
    if frac.is_empty() {
        return Ok(0);
    }
    if frac.len() > 6 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return Err(DbError::SqlPlan(format!(
            "invalid fractional seconds in timestamp {input:?} (max 6 digits)"
        )));
    }
    let mut micros = parse_uint(frac)?;
    for _ in 0..(6 - frac.len()) {
        micros *= 10;
    }
    Ok(micros)
}

/// Render an `i64`-microseconds timestamp back to canonical
/// `YYYY-MM-DD HH:MM:SS[.ffffff]` UTC text (fractional part only when nonzero).
/// Used by the REST DTO layer and any display path.
pub fn format_timestamp(micros: i64) -> String {
    let mut total_secs = micros.div_euclid(MICROS_PER_SEC);
    let frac = micros.rem_euclid(MICROS_PER_SEC);
    let days = total_secs.div_euclid(SECS_PER_DAY);
    total_secs = total_secs.rem_euclid(SECS_PER_DAY);
    let (y, mo, d) = civil_from_days(days);
    let hh = total_secs / 3600;
    let mm = (total_secs % 3600) / 60;
    let ss = total_secs % 60;
    let base = format!("{y:04}-{mo:02}-{d:02} {hh:02}:{mm:02}:{ss:02}");
    if frac == 0 {
        base
    } else {
        // Trim trailing zeros from the 6-digit fraction for a compact form.
        let frac_str = format!("{frac:06}");
        let trimmed = frac_str.trim_end_matches('0');
        format!("{base}.{trimmed}")
    }
}

/// Parse `'YYYY-MM-DD'` into days since the Unix epoch (P2.b). Rejects any
/// time-of-day suffix — that is `TIMESTAMP`, not `DATE`.
pub fn parse_date(input: &str) -> Result<i32> {
    let s = input.trim();
    let mut parts = s.split('-');
    let (y, mo, d) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(y), Some(mo), Some(d), None) => (parse_uint(y)?, parse_uint(mo)?, parse_uint(d)?),
        _ => {
            return Err(DbError::SqlPlan(format!(
                "invalid date {input:?}: expected 'YYYY-MM-DD'"
            )))
        }
    };
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) {
        return Err(DbError::SqlPlan(format!(
            "invalid date {input:?}: not a real date"
        )));
    }
    i32::try_from(days_from_civil(y, mo, d))
        .map_err(|_| DbError::SqlPlan(format!("date out of range: {input:?}")))
}

/// Render days-since-epoch back to `YYYY-MM-DD` (P2.b).
pub fn format_date(days: i32) -> String {
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}")
}

/// Parse `'HH:MM:SS[.ffffff]'` (seconds optional) into microseconds since
/// midnight (P2.b).
pub fn parse_time(input: &str) -> Result<i64> {
    let s = input.trim();
    let (hms, frac) = match s.split_once('.') {
        Some((hms, frac)) => (hms, frac),
        None => (s, ""),
    };
    let mut parts = hms.split(':');
    let (hh, mm, ss) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(m), Some(sec), None) => (parse_uint(h)?, parse_uint(m)?, parse_uint(sec)?),
        (Some(h), Some(m), None, None) => (parse_uint(h)?, parse_uint(m)?, 0),
        _ => {
            return Err(DbError::SqlPlan(format!(
                "invalid time {input:?}: expected 'HH:MM:SS[.ffffff]'"
            )))
        }
    };
    if hh > 23 || mm > 59 || ss > 59 {
        return Err(DbError::SqlPlan(format!("invalid time-of-day {input:?}")));
    }
    let micros = parse_fraction(frac, input)?;
    Ok((hh * 3600 + mm * 60 + ss) * MICROS_PER_SEC + micros)
}

/// Render micros-since-midnight back to `HH:MM:SS[.ffffff]` (P2.b).
pub fn format_time(micros: i64) -> String {
    let secs = micros / MICROS_PER_SEC;
    let frac = micros % MICROS_PER_SEC;
    let hh = secs / 3600;
    let mm = (secs % 3600) / 60;
    let ss = secs % 60;
    let base = format!("{hh:02}:{mm:02}:{ss:02}");
    if frac == 0 {
        base
    } else {
        let frac_str = format!("{frac:06}");
        format!("{base}.{}", frac_str.trim_end_matches('0'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_round_trips() {
        let d = parse_date("2024-03-14").unwrap();
        assert_eq!(format_date(d), "2024-03-14");
        assert_eq!(parse_date("1970-01-01").unwrap(), 0);
        assert!(parse_date("2024-03-14 00:00:00").is_err());
        assert!(parse_date("2023-02-29").is_err());
    }

    #[test]
    fn time_round_trips() {
        assert_eq!(parse_time("00:00:00").unwrap(), 0);
        let t = parse_time("13:45:30.5").unwrap();
        assert_eq!(format_time(t), "13:45:30.5");
        assert_eq!(
            parse_time("23:59:59").unwrap(),
            (23 * 3600 + 59 * 60 + 59) * 1_000_000
        );
        assert!(parse_time("24:00:00").is_err());
        assert_eq!(format_time(parse_time("09:05:00").unwrap()), "09:05:00");
    }

    #[test]
    fn epoch_round_trips() {
        assert_eq!(parse_timestamp("1970-01-01 00:00:00").unwrap(), 0);
        assert_eq!(format_timestamp(0), "1970-01-01 00:00:00");
    }

    #[test]
    fn date_only_is_midnight() {
        let micros = parse_timestamp("2024-01-01").unwrap();
        assert_eq!(format_timestamp(micros), "2024-01-01 00:00:00");
    }

    #[test]
    fn t_separator_and_z_suffix() {
        let a = parse_timestamp("2024-03-01T12:30:45").unwrap();
        let b = parse_timestamp("2024-03-01 12:30:45Z").unwrap();
        assert_eq!(a, b);
        assert_eq!(format_timestamp(a), "2024-03-01 12:30:45");
    }

    #[test]
    fn fractional_seconds_round_trip() {
        let micros = parse_timestamp("2024-01-01 00:00:00.5").unwrap();
        assert_eq!(micros % MICROS_PER_SEC, 500_000);
        assert_eq!(format_timestamp(micros), "2024-01-01 00:00:00.5");
        let full = parse_timestamp("2024-01-01 00:00:00.123456").unwrap();
        assert_eq!(format_timestamp(full), "2024-01-01 00:00:00.123456");
    }

    #[test]
    fn ordering_matches_chronology() {
        let earlier = parse_timestamp("2023-12-31 23:59:59").unwrap();
        let later = parse_timestamp("2024-01-01 00:00:00").unwrap();
        assert!(earlier < later);
        assert_eq!(later - earlier, MICROS_PER_SEC);
    }

    #[test]
    fn leap_day_valid_non_leap_day_rejected() {
        assert!(parse_timestamp("2024-02-29 00:00:00").is_ok());
        assert!(parse_timestamp("2023-02-29 00:00:00").is_err());
    }

    #[test]
    fn rejects_garbage_and_out_of_range() {
        assert!(parse_timestamp("not-a-date").is_err());
        assert!(parse_timestamp("2024-13-01").is_err());
        assert!(parse_timestamp("2024-01-01 25:00:00").is_err());
        assert!(parse_timestamp("2024-01-01 00:00:00.1234567").is_err());
    }

    #[test]
    fn pre_epoch_round_trips() {
        let micros = parse_timestamp("1969-12-31 23:59:59").unwrap();
        assert_eq!(micros, -MICROS_PER_SEC);
        assert_eq!(format_timestamp(micros), "1969-12-31 23:59:59");
    }
}
