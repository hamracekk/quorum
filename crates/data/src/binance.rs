//! Historical candles from Binance's public REST API (no API key required).

use std::thread;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use trader_core::Bar;

use crate::{DataError, Result};

const BASE_URL: &str = "https://api.binance.com";
const PAGE_LIMIT: usize = 1_000;

/// Download closed candles for `symbol` between `start` and `end`, paging as needed.
/// The still-forming last candle is dropped so it can't leak into a backtest.
pub fn fetch_klines(
    symbol: &str,
    interval: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Vec<Bar>> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("trader/0.1")
        .timeout(Duration::from_secs(30))
        .build()?;

    let end_ms = end.timestamp_millis();
    let mut cursor = start.timestamp_millis();
    let mut bars = Vec::new();

    while cursor < end_ms {
        let page: Vec<Vec<Value>> = client
            .get(format!("{BASE_URL}/api/v3/klines"))
            .query(&[
                ("symbol", symbol.to_owned()),
                ("interval", interval.to_owned()),
                ("startTime", cursor.to_string()),
                ("endTime", end_ms.to_string()),
                ("limit", PAGE_LIMIT.to_string()),
            ])
            .send()?
            .error_for_status()?
            .json()?;
        tracing::debug!(symbol, interval, rows = page.len(), "fetched klines page");

        let now_ms = Utc::now().timestamp_millis();
        for row in &page {
            if int_field(row, 6)? < now_ms {
                bars.push(parse_row(row)?);
            }
        }

        let Some(last) = page.last() else { break };
        if page.len() < PAGE_LIMIT {
            break;
        }
        cursor = int_field(last, 0)? + 1;
        // Stay well inside Binance's request-weight limits.
        thread::sleep(Duration::from_millis(200));
    }

    Ok(bars)
}

fn int_field(row: &[Value], i: usize) -> Result<i64> {
    row.get(i)
        .and_then(Value::as_i64)
        .ok_or_else(|| DataError::Malformed(format!("kline field {i} is not an integer")))
}

fn num_field(row: &[Value], i: usize) -> Result<f64> {
    row.get(i)
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| DataError::Malformed(format!("kline field {i} is not a number")))
}

fn parse_row(row: &[Value]) -> Result<Bar> {
    let open_ms = int_field(row, 0)?;
    Ok(Bar {
        ts: DateTime::from_timestamp_millis(open_ms)
            .ok_or_else(|| DataError::Malformed(format!("invalid open time {open_ms}")))?,
        open: num_field(row, 1)?,
        high: num_field(row, 2)?,
        low: num_field(row, 3)?,
        close: num_field(row, 4)?,
        volume: num_field(row, 5)?,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_a_kline_row() {
        let row = json!([
            1_700_000_000_000_i64,
            "100.5",
            "101",
            "99",
            "100.75",
            "12.5",
            1_700_000_059_999_i64
        ]);
        let row = row.as_array().expect("array");
        let bar = parse_row(row).expect("valid row");
        assert_eq!(bar.ts.timestamp_millis(), 1_700_000_000_000);
        assert!((bar.close - 100.75).abs() < 1e-12);
        assert!((bar.volume - 12.5).abs() < 1e-12);
    }

    #[test]
    fn rejects_non_numeric_prices() {
        let row = json!([0, "abc", "1", "1", "1", "1", 0]);
        let row = row.as_array().expect("array");
        assert!(matches!(parse_row(row), Err(DataError::Malformed(_))));
    }
}
