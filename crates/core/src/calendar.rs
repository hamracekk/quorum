//! Exchange calendars: trading days, holidays and regular session hours in UTC, for the venues
//! the equity and ETF slots trade on (Xetra, the London Stock Exchange, the New York exchanges).
//!
//! Rules, not data files: fixed holidays, Easter-based ones, weekday rules ("third Monday of
//! January") with each exchange's weekend substitutions, and a short list of one-off closures.
//! Daylight saving follows the EU (last Sunday of March to last Sunday of October) and US
//! (second Sunday of March to first Sunday of November) rules. Early closes (half days) are not
//! modelled: sessions are the regular ones.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, Utc, Weekday};
use serde::{Deserialize, Serialize};

/// An exchange whose calendar quorum knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Exchange {
    /// Deutsche Börse Xetra, Frankfurt (EUR listings of most UCITS ETFs).
    Xetra,
    /// London Stock Exchange (USD and GBP listings).
    Lse,
    /// The New York exchanges: NYSE, NYSE Arca, Nasdaq (one holiday calendar).
    Nyse,
}

impl Exchange {
    pub const ALL: [Self; 3] = [Self::Xetra, Self::Lse, Self::Nyse];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Xetra => "xetra",
            Self::Lse => "lse",
            Self::Nyse => "nyse",
        }
    }

    /// Whether the exchange trades on `day`.
    #[must_use]
    pub fn is_trading_day(self, day: NaiveDate) -> bool {
        !matches!(day.weekday(), Weekday::Sat | Weekday::Sun)
            && !self.holidays(day.year()).contains(&day)
    }

    /// The first trading day after `day`.
    #[must_use]
    pub fn next_trading_day(self, day: NaiveDate) -> NaiveDate {
        let mut next = day + Duration::days(1);
        while !self.is_trading_day(next) {
            next += Duration::days(1);
        }
        next
    }

    /// The regular session of `day` in UTC (open, close); `None` when it does not trade.
    #[must_use]
    pub fn session_utc(self, day: NaiveDate) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        if !self.is_trading_day(day) {
            return None;
        }
        // Local open and close, and the local offset from UTC in hours.
        let (open, close, offset) = match self {
            Self::Xetra => ((9, 0), (17, 30), if eu_summer(day) { 2 } else { 1 }),
            Self::Lse => ((8, 0), (16, 30), i64::from(eu_summer(day))),
            Self::Nyse => ((9, 30), (16, 0), if us_summer(day) { -4 } else { -5 }),
        };
        let at = |(h, m): (u32, u32)| {
            NaiveTime::from_hms_opt(h, m, 0)
                .map(|t| day.and_time(t).and_utc() - Duration::hours(offset))
        };
        Some((at(open)?, at(close)?))
    }

    /// Every full-day closure on a weekday in `year`.
    #[must_use]
    pub fn holidays(self, year: i32) -> BTreeSet<NaiveDate> {
        let mut out = BTreeSet::new();
        let Some(easter) = easter(year) else {
            return out;
        };
        let mut add = |d: Option<NaiveDate>| {
            if let Some(d) = d.filter(|d| !matches!(d.weekday(), Weekday::Sat | Weekday::Sun)) {
                out.insert(d);
            }
        };
        let date = |m, d| NaiveDate::from_ymd_opt(year, m, d);
        match self {
            Self::Xetra => {
                add(date(1, 1));
                add(easter.checked_sub_signed(Duration::days(2)));
                add(easter.checked_add_signed(Duration::days(1)));
                add(date(5, 1));
                for d in [24, 25, 26, 31] {
                    add(date(12, d));
                }
            }
            Self::Lse => {
                add(date(1, 1).map(monday_if_weekend));
                add(easter.checked_sub_signed(Duration::days(2)));
                add(easter.checked_add_signed(Duration::days(1)));
                // Early May and spring bank holidays moved or added in some years.
                match year {
                    2020 => add(date(5, 8)),
                    _ => add(nth_weekday(year, 5, Weekday::Mon, 1)),
                }
                match year {
                    2022 => {
                        add(date(6, 2));
                        add(date(6, 3));
                    }
                    _ => add(last_weekday(year, 5, Weekday::Mon)),
                }
                add(last_weekday(year, 8, Weekday::Mon));
                for (day, _) in uk_christmas(year) {
                    add(Some(day));
                }
                for (y, m, d) in LSE_ONE_OFF {
                    if *y == year {
                        add(date(*m, *d));
                    }
                }
            }
            Self::Nyse => {
                // New Year on a Saturday is not moved to the Friday before.
                add(date(1, 1).map(monday_if_sunday));
                add(nth_weekday(year, 1, Weekday::Mon, 3));
                add(nth_weekday(year, 2, Weekday::Mon, 3));
                add(easter.checked_sub_signed(Duration::days(2)));
                add(last_weekday(year, 5, Weekday::Mon));
                if year >= 2022 {
                    add(date(6, 19).map(nearest_weekday));
                }
                add(date(7, 4).map(nearest_weekday));
                add(nth_weekday(year, 9, Weekday::Mon, 1));
                add(nth_weekday(year, 11, Weekday::Thu, 4));
                add(date(12, 25).map(nearest_weekday));
                for (y, m, d) in NYSE_ONE_OFF {
                    if *y == year {
                        add(date(*m, *d));
                    }
                }
            }
        }
        out
    }
}

impl fmt::Display for Exchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Exchange {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|e| e.as_str() == s)
            .ok_or_else(|| format!("unknown exchange `{s}` (xetra, lse, nyse)"))
    }
}

/// Closures outside the rules: national days of mourning and events.
const NYSE_ONE_OFF: &[(i32, u32, u32)] = &[
    (2001, 9, 11),
    (2001, 9, 12),
    (2001, 9, 13),
    (2001, 9, 14),
    (2004, 6, 11),
    (2007, 1, 2),
    (2012, 10, 29),
    (2012, 10, 30),
    (2018, 12, 5),
    (2025, 1, 9),
];

const LSE_ONE_OFF: &[(i32, u32, u32)] = &[(2011, 4, 29), (2022, 9, 19), (2023, 5, 8)];

/// Easter Sunday (Gregorian, anonymous algorithm).
#[must_use]
pub fn easter(year: i32) -> Option<NaiveDate> {
    let golden = year.rem_euclid(19);
    let century = year.div_euclid(100);
    let within = year.rem_euclid(100);
    let (century_leaps, century_rest) = (century.div_euclid(4), century.rem_euclid(4));
    let correction = (century + 8).div_euclid(25);
    let moon = (century - correction + 1).div_euclid(3);
    let epact = (19 * golden + century - century_leaps - moon + 15).rem_euclid(30);
    let (within_leaps, within_rest) = (within.div_euclid(4), within.rem_euclid(4));
    let shift = (32 + 2 * century_rest + 2 * within_leaps - epact - within_rest).rem_euclid(7);
    let late = (golden + 11 * epact + 22 * shift).div_euclid(451);
    let month = (epact + shift - 7 * late + 114).div_euclid(31);
    let day = (epact + shift - 7 * late + 114).rem_euclid(31) + 1;
    NaiveDate::from_ymd_opt(year, u32::try_from(month).ok()?, u32::try_from(day).ok()?)
}

/// The `n`th `weekday` of `month` (1-based).
fn nth_weekday(year: i32, month: u32, weekday: Weekday, n: u32) -> Option<NaiveDate> {
    NaiveDate::from_weekday_of_month_opt(year, month, weekday, u8::try_from(n).ok()?)
}

/// The last `weekday` of `month`.
fn last_weekday(year: i32, month: u32, weekday: Weekday) -> Option<NaiveDate> {
    let next = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    }?;
    let mut day = next - Duration::days(1);
    while day.weekday() != weekday {
        day -= Duration::days(1);
    }
    Some(day)
}

fn monday_if_weekend(d: NaiveDate) -> NaiveDate {
    match d.weekday() {
        Weekday::Sat => d + Duration::days(2),
        Weekday::Sun => d + Duration::days(1),
        _ => d,
    }
}

fn monday_if_sunday(d: NaiveDate) -> NaiveDate {
    if d.weekday() == Weekday::Sun {
        d + Duration::days(1)
    } else {
        d
    }
}

/// US federal rule: Saturday → the Friday before, Sunday → the Monday after.
fn nearest_weekday(d: NaiveDate) -> NaiveDate {
    match d.weekday() {
        Weekday::Sat => d - Duration::days(1),
        Weekday::Sun => d + Duration::days(1),
        _ => d,
    }
}

/// UK Christmas and Boxing Day with their substitutes on the next free weekdays.
fn uk_christmas(year: i32) -> Vec<(NaiveDate, &'static str)> {
    let mut out: Vec<(NaiveDate, &'static str)> = Vec::new();
    for (day, name) in [(25, "christmas"), (26, "boxing day")] {
        let Some(mut d) = NaiveDate::from_ymd_opt(year, 12, day) else {
            continue;
        };
        while matches!(d.weekday(), Weekday::Sat | Weekday::Sun)
            || out.iter().any(|(taken, _)| *taken == d)
        {
            d += Duration::days(1);
        }
        out.push((d, name));
    }
    out
}

/// Whether `day` falls in EU summer time (the switch happens on Sundays, when nothing trades).
fn eu_summer(day: NaiveDate) -> bool {
    let year = day.year();
    match (
        last_weekday(year, 3, Weekday::Sun),
        last_weekday(year, 10, Weekday::Sun),
    ) {
        (Some(start), Some(end)) => day >= start && day < end,
        _ => false,
    }
}

/// Whether `day` falls in US daylight saving time.
fn us_summer(day: NaiveDate) -> bool {
    let year = day.year();
    match (
        nth_weekday(year, 3, Weekday::Sun, 2),
        nth_weekday(year, 11, Weekday::Sun, 1),
    ) {
        (Some(start), Some(end)) => day >= start && day < end,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).expect("valid")
    }

    #[test]
    fn easter_dates() {
        assert_eq!(easter(2024), Some(d(2024, 3, 31)));
        assert_eq!(easter(2025), Some(d(2025, 4, 20)));
        assert_eq!(easter(2026), Some(d(2026, 4, 5)));
    }

    #[test]
    fn nyse_2024_and_2025_holidays() {
        let h: Vec<NaiveDate> = Exchange::Nyse.holidays(2024).into_iter().collect();
        assert_eq!(
            h,
            [
                d(2024, 1, 1),
                d(2024, 1, 15),
                d(2024, 2, 19),
                d(2024, 3, 29),
                d(2024, 5, 27),
                d(2024, 6, 19),
                d(2024, 7, 4),
                d(2024, 9, 2),
                d(2024, 11, 28),
                d(2024, 12, 25),
            ]
        );
        // 2025: the national day of mourning on 9 January.
        assert!(!Exchange::Nyse.is_trading_day(d(2025, 1, 9)));
        // 4 July 2026 is a Saturday: observed on Friday the 3rd.
        assert!(!Exchange::Nyse.is_trading_day(d(2026, 7, 3)));
    }

    #[test]
    fn xetra_and_lse_2024_holidays() {
        let x: Vec<NaiveDate> = Exchange::Xetra.holidays(2024).into_iter().collect();
        assert_eq!(
            x,
            [
                d(2024, 1, 1),
                d(2024, 3, 29),
                d(2024, 4, 1),
                d(2024, 5, 1),
                d(2024, 12, 24),
                d(2024, 12, 25),
                d(2024, 12, 26),
                d(2024, 12, 31),
            ]
        );
        let l: Vec<NaiveDate> = Exchange::Lse.holidays(2024).into_iter().collect();
        assert_eq!(
            l,
            [
                d(2024, 1, 1),
                d(2024, 3, 29),
                d(2024, 4, 1),
                d(2024, 5, 6),
                d(2024, 5, 27),
                d(2024, 8, 26),
                d(2024, 12, 25),
                d(2024, 12, 26),
            ]
        );
        // 2021: Christmas on a Saturday, Boxing Day on a Sunday → Monday 27 and Tuesday 28.
        let l21 = Exchange::Lse.holidays(2021);
        assert!(l21.contains(&d(2021, 12, 27)) && l21.contains(&d(2021, 12, 28)));
    }

    #[test]
    fn sessions_follow_daylight_saving() {
        let at = |y, m, day, h, min| {
            d(y, m, day)
                .and_hms_opt(h, min, 0)
                .expect("valid")
                .and_utc()
        };
        assert_eq!(
            Exchange::Nyse.session_utc(d(2024, 7, 1)),
            Some((at(2024, 7, 1, 13, 30), at(2024, 7, 1, 20, 0)))
        );
        assert_eq!(
            Exchange::Nyse.session_utc(d(2024, 1, 2)),
            Some((at(2024, 1, 2, 14, 30), at(2024, 1, 2, 21, 0)))
        );
        assert_eq!(
            Exchange::Xetra.session_utc(d(2024, 7, 1)),
            Some((at(2024, 7, 1, 7, 0), at(2024, 7, 1, 15, 30)))
        );
        assert_eq!(
            Exchange::Lse.session_utc(d(2024, 1, 2)),
            Some((at(2024, 1, 2, 8, 0), at(2024, 1, 2, 16, 30)))
        );
        assert_eq!(Exchange::Xetra.session_utc(d(2024, 12, 24)), None);
    }

    #[test]
    fn the_next_trading_day_skips_weekends_and_holidays() {
        // Thursday 28 March 2024 → Tuesday 2 April at Xetra (Good Friday, Easter Monday).
        assert_eq!(
            Exchange::Xetra.next_trading_day(d(2024, 3, 28)),
            d(2024, 4, 2)
        );
        assert_eq!("lse".parse::<Exchange>(), Ok(Exchange::Lse));
    }
}
