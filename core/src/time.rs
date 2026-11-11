//! Calendar time as UTC epoch seconds: RFC 3339 on the wire, ISO-8601 from tenants. Civil-date
//! arithmetic is done here so the core needs no date crate.

use std::time::{SystemTime, UNIX_EPOCH};

const DAY: i64 = 86_400;

/// Seconds since the Unix epoch.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// A wire timestamp (core ↔ UI): `YYYY-MM-DDTHH:MM:SS[.fff]Z`, always UTC.
pub fn parse_rfc3339_utc(value: &str) -> Option<i64> {
    let (date, time) = value.strip_suffix('Z')?.split_once('T')?;
    Some(midnight(date)? + second_of_day(time)?)
}

pub fn format_rfc3339_utc(epoch: i64) -> String {
    let (year, month, day) = civil_from_days(epoch.div_euclid(DAY));
    let second = epoch.rem_euclid(DAY);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second / 3_600,
        second % 3_600 / 60,
        second % 60
    )
}

/// A tenant timestamp: `YYYY-MM-DD(T| )HH:MM:SS[.fff][Z|±HH:MM|±HHMM]`. Tenants that omit the
/// offset mean UTC.
pub fn parse_iso8601(value: &str) -> Option<i64> {
    let (date, rest) = value.trim().split_once(['T', ' '])?;
    let (time, offset) = split_offset(rest)?;
    Some(midnight(date)? + second_of_day(time)? - offset)
}

/// Split a trailing UTC offset off a time, returning the offset in seconds east of UTC.
fn split_offset(rest: &str) -> Option<(&str, i64)> {
    if let Some(time) = rest.strip_suffix('Z') {
        return Some((time, 0));
    }
    let Some(at) = rest.rfind(['+', '-']) else {
        return Some((rest, 0));
    };
    let (time, offset) = rest.split_at(at);
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    let [hours, minutes] = fields(&offset[1..], b':', [2, 2])
        .or_else(|| fields(&offset[1..], b':', [4]).map(|[hhmm]| [hhmm / 100, hhmm % 100]))?;
    // Civil offsets span −14:00..=+14:00, and the boundary hour carries no minutes.
    let valid = minutes < 60 && (hours < 14 || (hours == 14 && minutes == 0));
    valid.then_some((time, sign * (hours * 3_600 + minutes * 60)))
}

/// Epoch seconds at 00:00 UTC of a `YYYY-MM-DD` date that exists on the calendar.
fn midnight(date: &str) -> Option<i64> {
    let [year, month, day] = fields(date, b'-', [4, 2, 2])?;
    (1..=days_in_month(year, month))
        .contains(&day)
        .then(|| days_from_civil(year, month, day) * DAY)
}

/// Seconds into the day of an `HH:MM:SS[.fff]` clock reading; the fraction is validated and dropped.
fn second_of_day(time: &str) -> Option<i64> {
    let clock = match time.split_once('.') {
        Some((clock, fraction))
            if !fraction.is_empty() && fraction.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            clock
        }
        Some(_) => return None,
        None => time,
    };
    let [hour, minute, second] = fields(clock, b':', [2, 2, 2])?;
    (hour < 24 && minute < 60 && second < 60).then_some(hour * 3_600 + minute * 60 + second)
}

/// Fixed-width decimal fields joined by `separator`: `2026-01-31` is `fields(_, b'-', [4, 2, 2])`.
fn fields<const N: usize>(text: &str, separator: u8, widths: [usize; N]) -> Option<[i64; N]> {
    let mut rest = text.as_bytes();
    let mut values = [0; N];
    for (index, width) in widths.into_iter().enumerate() {
        if index > 0 {
            rest = rest.strip_prefix(&[separator])?;
        }
        let (digits, tail) = rest.split_at_checked(width)?;
        if !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        values[index] = digits
            .iter()
            .fold(0, |value, digit| value * 10 + i64::from(digit - b'0'));
        rest = tail;
    }
    rest.is_empty().then_some(values)
}

/// Zero for a month that does not exist, so a range check on the day rejects it too.
fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

// Howard Hinnant's civil calendar conversions, with day 0 = 1970-01-01.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = month_index + if month_index < 10 { 3 } else { -9 };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEW_YEAR_2021: i64 = 1_609_459_200;

    #[test]
    fn rfc3339_round_trips_a_leap_day_and_a_negative_epoch() {
        for value in ["1969-12-31T23:59:59Z", "2028-02-29T12:34:56.1234567Z"] {
            let epoch = parse_rfc3339_utc(value).unwrap();
            assert_eq!(parse_rfc3339_utc(&format_rfc3339_utc(epoch)), Some(epoch));
        }
        assert_eq!(format_rfc3339_utc(NEW_YEAR_2021), "2021-01-01T00:00:00Z");
        assert_eq!(format_rfc3339_utc(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn rfc3339_requires_utc_and_a_real_date() {
        assert_eq!(parse_rfc3339_utc("2026-02-29T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_utc("2026-01-01T00:00:00+00:00"), None);
        assert_eq!(parse_rfc3339_utc("2026-01-01 00:00:00Z"), None);
        assert_eq!(parse_rfc3339_utc("2026-1-1T0:0:0Z"), None);
    }

    #[test]
    fn iso8601_reads_utc_offsets_fractions_and_a_space_separator() {
        for value in [
            "2021-01-01T00:00:00Z",
            "2021-01-01T08:00:00+08:00",
            "2021-01-01T08:00:00+0800",
            "2020-12-31T19:00:00-05:00",
            "2021-01-01 00:00:00.500Z",
            "2021-01-01T00:00:00",
        ] {
            assert_eq!(parse_iso8601(value), Some(NEW_YEAR_2021), "{value}");
        }
        assert_eq!(parse_iso8601("not-a-date"), None);
    }

    #[test]
    fn iso8601_rejects_impossible_dates_times_and_offsets() {
        assert!(parse_iso8601("2024-02-29T00:00:00Z").is_some());
        assert!(parse_iso8601("2024-01-01T00:00:00+14:00").is_some());
        for value in [
            "2023-02-29T00:00:00Z",
            "2024-04-31T00:00:00Z",
            "2024-13-01T00:00:00Z",
            "2024-01-01T24:00:00Z",
            "2024-01-01T23:60:00Z",
            "2024-01-01T23:59:60Z",
            "2024-01-01T00:00:00.Z",
            "2024-01-01T00:00:00+24:00",
            "2024-01-01T00:00:00+14:01",
            "2024-01-01T00:00:00+2360",
            "2024-01-01T00:00:00+08:60",
            "2024-01-01T00:00:00+8",
        ] {
            assert_eq!(parse_iso8601(value), None, "{value}");
        }
    }
}
