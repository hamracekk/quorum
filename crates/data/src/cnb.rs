//! Czech National Bank exchange-rate fixing (ČNB): the official daily CZK rates, the ones Czech
//! tax uses. Public, keyless; one text file per year:
//!
//! ```text
//! Date|1 AUD|…|100 JPY|…|1 USD|…
//! 02.01.2024|15.278|…|15.862|…|22.526|…
//! ```
//!
//! Columns change between years (currencies come and go), so each file is read by its header.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::NaiveDate;

use crate::{DataError, Result};

const YEAR_URL: &str = "https://www.cnb.cz/en/financial-markets/foreign-exchange-market/central-bank-exchange-rate-fixing/central-bank-exchange-rate-fixing/year.txt";

/// The column of `currency` in a header line and its unit amount (1 or 100).
fn column(header: &str, currency: &str) -> Option<(usize, f64)> {
    header.split('|').enumerate().find_map(|(i, h)| {
        let (amount, code) = h.trim().split_once(' ')?;
        (code == currency)
            .then(|| amount.parse::<f64>().ok().map(|a| (i, a)))
            .flatten()
    })
}

/// CZK per one unit of `currency` on each fixing date of one year's file. A header repeated
/// mid-file (the columns changed) is read again.
pub fn parse_year(text: &str, currency: &str) -> Result<BTreeMap<NaiveDate, f64>> {
    let mut current: Option<(usize, f64)> = None;
    let mut seen_header = false;
    let mut out = BTreeMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        if line.starts_with("Date") {
            seen_header = true;
            current = column(line, currency);
            continue;
        }
        let Some((col, amount)) = current else {
            continue;
        };
        let fields: Vec<&str> = line.split('|').collect();
        let (Some(date), Some(rate)) = (fields.first(), fields.get(col)) else {
            continue;
        };
        let Ok(date) = NaiveDate::parse_from_str(date.trim(), "%d.%m.%Y") else {
            continue;
        };
        let rate: f64 = rate
            .trim()
            .replace(',', ".")
            .parse()
            .map_err(|e| DataError::Malformed(format!("ČNB {date}: {e}")))?;
        if rate > 0.0 && amount > 0.0 {
            out.insert(date, rate / amount);
        }
    }
    if !seen_header {
        return Err(DataError::Malformed("ČNB: no header".to_owned()));
    }
    if out.is_empty() {
        return Err(DataError::Malformed(format!("ČNB: no {currency} rates")));
    }
    Ok(out)
}

/// CZK per one unit of `currency` for every fixing date in `from_year..=to_year`.
pub async fn rates(
    currency: &str,
    from_year: i32,
    to_year: i32,
) -> Result<BTreeMap<NaiveDate, f64>> {
    let client = reqwest::Client::builder()
        .user_agent("quorum/0.1")
        .timeout(Duration::from_secs(30))
        .build()?;
    let mut out = BTreeMap::new();
    for year in from_year..=to_year {
        let text = client
            .get(YEAR_URL)
            .query(&[("year", year.to_string())])
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        out.extend(parse_year(&text, currency)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "Date|1 EUR|100 JPY|1 USD\n\
                          02.01.2024|24.685|15.862|22.526\n\
                          03.01.2024|24.675|15.809|22.600\n";

    #[test]
    fn rates_are_read_by_the_header_and_per_unit() {
        let usd = parse_year(SAMPLE, "USD").expect("usd");
        let day = NaiveDate::from_ymd_opt(2024, 1, 3).expect("valid");
        assert!((usd[&day] - 22.600).abs() < 1e-12);
        let jpy = parse_year(SAMPLE, "JPY").expect("jpy");
        assert!((jpy[&day] - 0.158_09).abs() < 1e-12, "per one yen");
        assert!(parse_year(SAMPLE, "GBP").is_err());
    }

    #[test]
    fn a_header_repeated_mid_year_moves_the_column() {
        let text =
            "Date|1 EUR|1 USD\n02.01.2024|24.0|22.0\nDate|1 USD|1 EUR\n03.01.2024|23.0|25.0\n";
        let usd = parse_year(text, "USD").expect("usd");
        let day = |d| NaiveDate::from_ymd_opt(2024, 1, d).expect("valid");
        assert!((usd[&day(2)] - 22.0).abs() < 1e-12);
        assert!((usd[&day(3)] - 23.0).abs() < 1e-12);
    }
}
