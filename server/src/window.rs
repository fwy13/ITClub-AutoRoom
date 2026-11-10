//! Reservation horizon expressed in calendar days in Vietnam (UTC+07).
//! All callers use this shared rule; never rely on the VPS's local timezone.
use chrono::{DateTime, Duration, NaiveDate, Utc};

pub const IMMEDIATE_LAST_DAY: i64 = 13;
pub const ADVANCE_FIRST_DAY: i64 = 14;
pub const ADVANCE_LAST_DAY: i64 = 44; // B+30 where B=A+14, inclusive.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window { Immediate, Advance, Outside }

pub fn today_vn(now: DateTime<Utc>) -> NaiveDate {
    (now + Duration::hours(7)).date_naive()
}

pub fn classify(target: NaiveDate, today: NaiveDate) -> Window {
    match (target - today).num_days() {
        0..=IMMEDIATE_LAST_DAY => Window::Immediate,
        ADVANCE_FIRST_DAY..=ADVANCE_LAST_DAY => Window::Advance,
        _ => Window::Outside,
    }
}

/// First moment a date enters the public 13-day horizon (00:00 ICT).
/// This is a *checking* start, not an assertion about CTU's official opening hour.
pub fn starts_at(target: NaiveDate) -> Option<i64> {
    let date = target.checked_sub_signed(Duration::days(IMMEDIATE_LAST_DAY))?;
    let midnight = date.and_hms_opt(0, 0, 0)?;
    Some(midnight.and_utc().timestamp() - 7 * 3600)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_inclusive_boundaries() {
        let a = NaiveDate::from_ymd_opt(2026, 11, 10).unwrap();
        for d in [0, 1, 13] { assert_eq!(classify(a + Duration::days(d), a), Window::Immediate); }
        for d in [14, 15, 44] { assert_eq!(classify(a + Duration::days(d), a), Window::Advance); }
        for d in [-1, 45, 90] { assert_eq!(classify(a + Duration::days(d), a), Window::Outside); }
    }
    #[test]
    fn advance_start_crosses_month_and_year() {
        let target = NaiveDate::from_ymd_opt(2027, 1, 5).unwrap();
        let utc = DateTime::<Utc>::from_timestamp(starts_at(target).unwrap(), 0).unwrap();
        assert_eq!(utc.format("%Y-%m-%d %H:%M").to_string(), "2026-12-22 17:00");
    }
    #[test]
    fn day_boundary_is_in_vietnam() {
        let utc = DateTime::<Utc>::from_naive_utc_and_offset(
            NaiveDate::from_ymd_opt(2026, 11, 10).unwrap().and_hms_opt(17, 30, 0).unwrap(), Utc);
        assert_eq!(today_vn(utc), NaiveDate::from_ymd_opt(2026, 11, 11).unwrap());
    }
}
