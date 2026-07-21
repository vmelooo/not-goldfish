//! Shared epoch → civil-date conversion and formatting.
//!
//! `civil_from_days` was independently reimplemented in `ng-hook` and
//! `ng-cli` (Howard Hinnant's days-from-civil algorithm, copy-pasted per
//! call site); a date bug fixed in one copy silently stayed broken in the
//! others. This module is the one source of truth: `ng-hook` (injeção) e
//! `ng-cli` já consomem daqui; `ng-sessions` mantém a própria conversão
//! (`days_from_civil`) por ser crate de mais baixo nível — não depende de
//! `ng-core`.

/// Converts `z`, a day count relative to 1970-01-01 (i.e. `epoch / 86_400`),
/// into a proleptic-Gregorian `(year, month, day)` civil date. Howard
/// Hinnant's `civil_from_days` algorithm — exact for the full `i64` range,
/// no floating point.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// Inverse of [`civil_from_days`]: proleptic-Gregorian `(year, month, day)`
/// civil date → day count relative to 1970-01-01. Same Howard Hinnant
/// algorithm family (`days_from_civil`), exact, no floating point.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let m = m as i64;
    let d = d as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse `YYYY-MM-DD` into the Unix epoch (seconds) of that day's midnight
/// UTC. `None` for anything that isn't a well-formed calendar date —
/// round-trips through [`civil_from_days`] so impossible dates (2026-02-30,
/// month 13) are rejected instead of silently normalized.
pub fn parse_date(s: &str) -> Option<i64> {
    let mut parts = s.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let days = days_from_civil(y, m, d);
    if civil_from_days(days) != (y, m, d) {
        return None;
    }
    Some(days * 86_400)
}

/// Day-precision date, `YYYY-MM-DD`, from a Unix epoch (seconds). Uses
/// Euclidean division so negative epochs (pre-1970) still land on the
/// correct calendar day instead of rounding toward zero.
pub fn fmt_date(epoch: i64) -> String {
    let (y, m, d) = civil_from_days(epoch.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Minute-precision timestamp, `YYYY-MM-DD HH:MMZ`, from a Unix epoch
/// (seconds).
pub fn fmt_datetime(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let secs_of_day = epoch.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_epoch_zero_is_unix_epoch_day() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn civil_from_days_known_millennium_boundary() {
        // 946684800 / 86400 = 10957 exactly: 2000-01-01T00:00:00Z.
        assert_eq!(civil_from_days(10957), (2000, 1, 1));
    }

    #[test]
    fn civil_from_days_handles_pre_epoch_dates() {
        // One day before the epoch: 1969-12-31.
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn fmt_date_epoch_zero() {
        assert_eq!(fmt_date(0), "1970-01-01");
    }

    #[test]
    fn fmt_date_known_millennium_boundary() {
        assert_eq!(fmt_date(946_684_800), "2000-01-01");
    }

    #[test]
    fn fmt_date_negative_epoch_rounds_toward_the_correct_day() {
        // -1 second is still 1969-12-31, not 1970-01-01 (which a truncating
        // `epoch / 86_400` would incorrectly produce).
        assert_eq!(fmt_date(-1), "1969-12-31");
    }

    #[test]
    fn days_from_civil_is_the_inverse_of_civil_from_days() {
        for days in [-1_000_000, -1, 0, 10_957, 20_655, 1_000_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "round-trip de {days}");
        }
    }

    #[test]
    fn parse_date_accepts_a_valid_civil_date() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("2000-01-01"), Some(946_684_800));
    }

    #[test]
    fn parse_date_rejects_malformed_and_impossible_dates() {
        for bad in ["", "2026", "2026-13-01", "2026-02-30", "2026-01-00", "hoje"] {
            assert_eq!(parse_date(bad), None, "{bad:?} deveria ser rejeitada");
        }
    }

    #[test]
    fn fmt_datetime_epoch_zero() {
        assert_eq!(fmt_datetime(0), "1970-01-01 00:00Z");
    }

    #[test]
    fn fmt_datetime_formats_hours_and_minutes() {
        // 1h 1m 1s past midnight.
        assert_eq!(fmt_datetime(3_661), "1970-01-01 01:01Z");
    }
}
