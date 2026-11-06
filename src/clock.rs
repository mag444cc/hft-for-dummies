//! What time is it in New York, and what day?
//!
//! The busiest, jumpiest part of the crypto day is around the US stock
//! market open, 09:30 New York time. In UTC that moves with daylight saving
//! (14:30 in winter, 13:30 in summer), so the bot works in New York time.
//! US daylight saving runs from the second Sunday in March, 02:00 local, to
//! the first Sunday in November, 02:00 local. No time-zone library needed.

/// Days since 1970-01-01 to (year, month 1-12, day 1-31). The usual
/// "civil from days" calculation.
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// (year, month, day) to days since 1970-01-01.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Weekday of a day number: 0 = Monday ... 6 = Sunday (1970-01-01 was a Thursday).
fn weekday(days: i64) -> i64 {
    (days + 3).rem_euclid(7)
}

/// The day number of the nth Sunday (1 = first) of a month.
fn nth_sunday(year: i64, month: i64, n: i64) -> i64 {
    let first = days_from_civil(year, month, 1);
    first + (6 - weekday(first)).rem_euclid(7) + 7 * (n - 1)
}

/// Is US daylight saving on at this moment (unix seconds)?
fn us_daylight_saving(unix_secs: i64) -> bool {
    let (year, _, _) = civil(unix_secs.div_euclid(86_400));
    // 02:00 local: 07:00 UTC in March (still on EST), 06:00 UTC in November (on EDT).
    let start = nth_sunday(year, 3, 2) * 86_400 + 7 * 3600;
    let end = nth_sunday(year, 11, 1) * 86_400 + 6 * 3600;
    unix_secs >= start && unix_secs < end
}

/// New York local time: (weekday 0 = Monday, minutes after midnight).
pub fn new_york(unix_secs: i64) -> (i64, i64) {
    let offset = if us_daylight_saving(unix_secs) {
        -4
    } else {
        -5
    };
    let local = unix_secs + offset * 3600;
    let day = local.div_euclid(86_400);
    (weekday(day), local.rem_euclid(86_400) / 60)
}

/// Is it Saturday or Sunday in UTC?
pub fn utc_weekend(unix_secs: i64) -> bool {
    weekday(unix_secs.div_euclid(86_400)) >= 5
}

/// "HH:MM" to minutes after midnight.
pub fn parse_hhmm(s: &str) -> Option<i64> {
    let (h, m) = s.split_once(':')?;
    let (h, m): (i64, i64) = (h.trim().parse().ok()?, m.trim().parse().ok()?);
    ((0..24).contains(&h) && (0..60).contains(&m)).then_some(h * 60 + m)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unix seconds for a UTC date and time.
    fn utc(y: i64, mo: i64, d: i64, h: i64, mi: i64) -> i64 {
        days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60
    }

    #[test]
    fn dates_round_trip() {
        for days in [-1000, 0, 1, 20_000, 20_763, 25_000] {
            let (y, m, d) = civil(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(weekday(0), 3, "1970-01-01 was a Thursday");
    }

    #[test]
    fn winter_new_york_is_utc_minus_five() {
        // Friday 2026-11-06 14:00 UTC = 09:00 in New York (EST).
        assert_eq!(new_york(utc(2026, 11, 6, 14, 0)), (4, 9 * 60));
    }

    #[test]
    fn summer_new_york_is_utc_minus_four() {
        // Wednesday 2026-07-01 13:00 UTC = 09:00 in New York (EDT).
        assert_eq!(new_york(utc(2026, 7, 1, 13, 0)), (2, 9 * 60));
    }

    #[test]
    fn daylight_saving_switches_on_the_right_sundays() {
        // 2026: starts Sunday 8 March 07:00 UTC, ends Sunday 1 November 06:00 UTC.
        assert!(!us_daylight_saving(utc(2026, 3, 8, 6, 59)));
        assert!(us_daylight_saving(utc(2026, 3, 8, 7, 0)));
        assert!(us_daylight_saving(utc(2026, 11, 1, 5, 59)));
        assert!(!us_daylight_saving(utc(2026, 11, 1, 6, 0)));
    }

    #[test]
    fn weekends_and_times() {
        assert!(utc_weekend(utc(2026, 11, 7, 12, 0)), "a Saturday");
        assert!(!utc_weekend(utc(2026, 11, 6, 23, 59)), "a Friday");
        assert_eq!(parse_hhmm("09:00"), Some(540));
        assert_eq!(parse_hhmm("11:30"), Some(690));
        assert_eq!(parse_hhmm("25:00"), None);
        assert_eq!(parse_hhmm("nine"), None);
    }
}
