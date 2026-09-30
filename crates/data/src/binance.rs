//! Historical candles from Binance's public REST API (no API key required).

use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use trader_core::Bar;

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

    loop {
        let page: Vec<Vec<Value>> = client
            .get(format!("{BASE_URL}/api/v3/klines"))
            .query(&[
                ("symbol", symbol.to_string()),
                ("interval", interval.to_string()),
                ("startTime", cursor.to_string()),
                ("endTime", end_ms.to_string()),
                ("limit", PAGE_LIMIT.to_string()),
            ])
            .send()?
            .error_for_status()
            .context("Binance rejected the klines request (check symbol/interval)")?
            .json()?;

        let now_ms = Utc::now().timestamp_millis();
        for row in &page {
            let close_ms = row
                .get(6)
                .and_then(Value::as_i64)
                .context("missing close time")?;
            if close_ms < now_ms {
                bars.push(parse_row(row)?);
            }
        }

        let Some(last) = page.last() else { break };
        if page.len() < PAGE_LIMIT {
            break;
        }
        cursor = last
            .first()
            .and_then(Value::as_i64)
            .context("missing open time")?
            + 1;
        // Stay well inside Binance's request-weight limits.
        thread::sleep(Duration::from_millis(200));
    }

    Ok(bars)
}

fn parse_row(row: &[Value]) -> Result<Bar> {
    let open_ms = row
        .first()
        .and_then(Value::as_i64)
        .context("missing open time")?;
    let num = |i: usize| -> Result<f64> {
        row.get(i)
            .and_then(Value::as_str)
            .with_context(|| format!("missing field {i}"))?
            .parse::<f64>()
            .with_context(|| format!("field {i} is not a number"))
    };
    Ok(Bar {
        ts: DateTime::from_timestamp_millis(open_ms).context("invalid open time")?,
        open: num(1)?,
        high: num(2)?,
        low: num(3)?,
        close: num(4)?,
        volume: num(5)?,
    })
}
