//! Binance spot **testnet** trading (T-7): signed account, rules and market-order requests.
//!
//! The client talks only to `testnet.binance.vision` (or a loopback mock in tests): any other
//! host is refused at construction, so it can never reach the live exchange — live trading is
//! M10. Requests are signed with HMAC-SHA256 over the query string, as Binance specifies; the
//! key and secret are never logged and their `Debug` output is redacted.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use chrono::Utc;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;

use crate::{DataError, Result};

/// The only exchange host this client talks to.
pub const TESTNET_HOST: &str = "testnet.binance.vision";
const TESTNET_URL: &str = "https://testnet.binance.vision";
const RECV_WINDOW_MS: u64 = 5_000;

/// API key and secret (from the OS keychain; never logged).
#[derive(Clone)]
pub struct Credentials {
    pub key: String,
    pub secret: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials { key: <redacted>, secret: <redacted> }")
    }
}

/// Trading rules of one symbol (`LOT_SIZE`, `NOTIONAL` / `MIN_NOTIONAL` filters).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SymbolRules {
    pub symbol: String,
    pub base_asset: String,
    pub quote_asset: String,
    /// Quantity increment.
    pub step_size: f64,
    pub min_qty: f64,
    /// Smallest order value in the quote asset.
    pub min_notional: f64,
    /// Price increment (`PRICE_FILTER`); `0` when unknown.
    #[serde(default)]
    pub tick_size: f64,
}

impl SymbolRules {
    /// `qty` rounded down to the step size.
    pub fn floor_qty(&self, qty: f64) -> f64 {
        if self.step_size <= 0.0 || qty <= 0.0 {
            return qty.max(0.0);
        }
        // A small epsilon keeps 0.3 / 0.1 from rounding down to 2 steps.
        let steps = (qty / self.step_size + 1e-9).floor();
        steps * self.step_size
    }

    /// `price` rounded down to the tick size and printed with its decimals.
    pub fn format_price(&self, price: f64) -> String {
        if self.tick_size <= 0.0 {
            return format!("{price:.8}");
        }
        let steps = (price / self.tick_size + 1e-9).floor();
        let decimals = format!("{}", self.tick_size)
            .split_once('.')
            .map_or(0, |(_, d)| d.trim_end_matches('0').len());
        format!("{:.decimals$}", steps * self.tick_size)
    }

    /// `qty` as the exchange expects it: at most as many decimals as the step size has.
    pub fn format_qty(&self, qty: f64) -> String {
        let decimals = format!("{}", self.step_size)
            .split_once('.')
            .map_or(0, |(_, d)| d.trim_end_matches('0').len());
        format!("{qty:.decimals$}")
    }
}

/// What the account may do and what it holds.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Account {
    pub can_trade: bool,
    pub can_withdraw: bool,
    /// Asset → free + locked.
    pub balances: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Buy => "BUY",
            Self::Sell => "SELL",
        }
    }

    /// `1` for a buy, `-1` for a sell.
    pub const fn sign(self) -> f64 {
        match self {
            Self::Buy => 1.0,
            Self::Sell => -1.0,
        }
    }
}

/// An order as the exchange reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderResult {
    pub order_id: i64,
    pub client_order_id: String,
    pub status: String,
    pub executed_qty: f64,
    /// Quote spent or received.
    pub quote_qty: f64,
    /// Commission per asset.
    pub fees: BTreeMap<String, f64>,
}

impl OrderResult {
    /// Average fill price, if anything filled.
    pub fn avg_price(&self) -> Option<f64> {
        (self.executed_qty > 0.0).then(|| self.quote_qty / self.executed_qty)
    }
}

/// HMAC-SHA256 of `query` with `secret`, hex-encoded (Binance's `signature` parameter).
pub fn sign(secret: &str, query: &str) -> String {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret.as_bytes())
        .unwrap_or_else(|_| unreachable!("HMAC takes any key length"));
    mac.update(query.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Whether `url` is the testnet or a loopback mock (tests); everything else is refused.
pub fn allowed_url(url: &str) -> bool {
    let (secure, rest) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
        (Some(rest), _) => (true, rest),
        (None, Some(rest)) => (false, rest),
        (None, None) => return false,
    };
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.contains('@') {
        return false;
    }
    let host = authority
        .rsplit_once(':')
        .filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit()))
        .map_or(authority, |(h, _)| h);
    let loopback = matches!(host, "127.0.0.1" | "localhost" | "[::1]");
    (secure && host == TESTNET_HOST) || (!secure && loopback)
}

/// Signed client for Binance spot testnet.
#[derive(Debug, Clone)]
pub struct BinanceTestnet {
    client: reqwest::Client,
    base_url: String,
    credentials: Credentials,
}

impl BinanceTestnet {
    pub fn new(credentials: Credentials) -> Result<Self> {
        Self::with_base_url(TESTNET_URL, credentials)
    }

    /// Point at a loopback mock (tests). Any host other than the testnet or loopback is refused.
    pub fn with_base_url(base_url: impl Into<String>, credentials: Credentials) -> Result<Self> {
        let base_url = base_url.into();
        if !allowed_url(&base_url) {
            return Err(DataError::Malformed(format!(
                "refusing {base_url}: the executor trades on {TESTNET_HOST} only (live is M10)"
            )));
        }
        let client = reqwest::Client::builder()
            .user_agent("trader/0.1")
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
            credentials,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn signed_query(&self, params: &[(&str, String)]) -> String {
        let mut query: Vec<String> = params.iter().map(|(k, v)| format!("{k}={v}")).collect();
        query.push(format!("recvWindow={RECV_WINDOW_MS}"));
        query.push(format!("timestamp={}", Utc::now().timestamp_millis()));
        let query = query.join("&");
        let signature = sign(&self.credentials.secret, &query);
        format!("{query}&signature={signature}")
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Value> {
        let response = request
            .header("X-MBX-APIKEY", &self.credentials.key)
            .send()
            .await?;
        let status = response.status();
        let body: Value = response.json().await?;
        if status.is_success() {
            return Ok(body);
        }
        Err(DataError::Malformed(format!(
            "testnet {status}: {} (code {})",
            body["msg"].as_str().unwrap_or("no message"),
            body["code"]
        )))
    }

    async fn signed_get(&self, path: &str, params: &[(&str, String)]) -> Result<Value> {
        let url = format!("{}{path}?{}", self.base_url, self.signed_query(params));
        self.send(self.client.get(url)).await
    }

    /// The account's permissions and balances.
    pub async fn account(&self) -> Result<Account> {
        let body = self.signed_get("/api/v3/account", &[]).await?;
        let flag = |name: &str| body[name].as_bool().unwrap_or(false);
        let mut balances = BTreeMap::new();
        for b in body["balances"].as_array().into_iter().flatten() {
            let total = number(&b["free"])? + number(&b["locked"])?;
            if let Some(asset) = b["asset"].as_str()
                && total > 0.0
            {
                balances.insert(asset.to_owned(), total);
            }
        }
        Ok(Account {
            can_trade: flag("canTrade"),
            can_withdraw: flag("canWithdraw"),
            balances,
        })
    }

    /// Trading rules of `symbols` (public).
    pub async fn rules(&self, symbols: &[String]) -> Result<Vec<SymbolRules>> {
        let list = serde_json::to_string(symbols)
            .map_err(|e| DataError::Malformed(format!("symbols: {e}")))?;
        let body: Value = self
            .client
            .get(format!("{}/api/v3/exchangeInfo", self.base_url))
            .query(&[("symbols", list)])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        body["symbols"]
            .as_array()
            .ok_or_else(|| DataError::Malformed("exchangeInfo without symbols".into()))?
            .iter()
            .map(parse_rules)
            .collect()
    }

    /// Place a market order for `qty` of the base asset, tagged `client_id` (idempotency key).
    pub async fn market_order(
        &self,
        symbol: &str,
        side: OrderSide,
        qty: &str,
        client_id: &str,
    ) -> Result<OrderResult> {
        let query = self.signed_query(&[
            ("symbol", symbol.to_owned()),
            ("side", side.as_str().to_owned()),
            ("type", "MARKET".to_owned()),
            ("quantity", qty.to_owned()),
            ("newClientOrderId", client_id.to_owned()),
            ("newOrderRespType", "FULL".to_owned()),
        ]);
        let url = format!("{}/api/v3/order?{query}", self.base_url);
        parse_order(&self.send(self.client.post(url)).await?)
    }

    /// Rest a stop-loss-limit sell of `qty` at the exchange: when the price trades at `stop`, a
    /// limit order at `limit` is placed — it works while this app is offline (S-4).
    pub async fn stop_loss(
        &self,
        symbol: &str,
        qty: &str,
        stop: &str,
        limit: &str,
        client_id: &str,
    ) -> Result<OrderResult> {
        let query = self.signed_query(&[
            ("symbol", symbol.to_owned()),
            ("side", "SELL".to_owned()),
            ("type", "STOP_LOSS_LIMIT".to_owned()),
            ("timeInForce", "GTC".to_owned()),
            ("quantity", qty.to_owned()),
            ("price", limit.to_owned()),
            ("stopPrice", stop.to_owned()),
            ("newClientOrderId", client_id.to_owned()),
            ("newOrderRespType", "RESULT".to_owned()),
        ]);
        let url = format!("{}/api/v3/order?{query}", self.base_url);
        parse_order(&self.send(self.client.post(url)).await?)
    }

    /// Cancel the order tagged `client_id`; `None` when the exchange no longer has it open.
    pub async fn cancel(&self, symbol: &str, client_id: &str) -> Result<Option<OrderResult>> {
        let query = self.signed_query(&[
            ("symbol", symbol.to_owned()),
            ("origClientOrderId", client_id.to_owned()),
        ]);
        let url = format!("{}/api/v3/order?{query}", self.base_url);
        match self.send(self.client.delete(url)).await {
            Ok(body) => parse_order(&body).map(Some),
            // -2011: unknown order (filled, cancelled or never placed).
            Err(DataError::Malformed(msg)) if msg.contains("-2011") => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The order tagged `client_id`, if the exchange knows it (to resolve an uncertain submit).
    pub async fn order(&self, symbol: &str, client_id: &str) -> Result<Option<OrderResult>> {
        let params = [
            ("symbol", symbol.to_owned()),
            ("origClientOrderId", client_id.to_owned()),
        ];
        match self.signed_get("/api/v3/order", &params).await {
            Ok(body) => parse_order(&body).map(Some),
            // -2013: the order does not exist.
            Err(DataError::Malformed(msg)) if msg.contains("-2013") => Ok(None),
            Err(e) => Err(e),
        }
    }
}

fn number(value: &Value) -> Result<f64> {
    match value {
        Value::String(s) => s
            .parse()
            .map_err(|_| DataError::Malformed(format!("not a number: {s}"))),
        Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| DataError::Malformed(format!("not a number: {n}"))),
        other => Err(DataError::Malformed(format!("not a number: {other}"))),
    }
}

fn text(value: &Value, name: &str) -> Result<String> {
    value[name]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| DataError::Malformed(format!("missing `{name}`")))
}

fn parse_rules(symbol: &Value) -> Result<SymbolRules> {
    let filters = symbol["filters"].as_array().cloned().unwrap_or_default();
    let filter = |kind: &str| filters.iter().find(|f| f["filterType"] == kind);
    let lot =
        filter("LOT_SIZE").ok_or_else(|| DataError::Malformed("symbol without LOT_SIZE".into()))?;
    let min_notional = match filter("NOTIONAL").or_else(|| filter("MIN_NOTIONAL")) {
        Some(f) => number(&f["minNotional"])?,
        None => 0.0,
    };
    let tick_size = match filter("PRICE_FILTER") {
        Some(f) => number(&f["tickSize"])?,
        None => 0.0,
    };
    Ok(SymbolRules {
        symbol: text(symbol, "symbol")?,
        base_asset: text(symbol, "baseAsset")?,
        quote_asset: text(symbol, "quoteAsset")?,
        step_size: number(&lot["stepSize"])?,
        min_qty: number(&lot["minQty"])?,
        min_notional,
        tick_size,
    })
}

fn parse_order(body: &Value) -> Result<OrderResult> {
    let mut fees = BTreeMap::new();
    for fill in body["fills"].as_array().into_iter().flatten() {
        let asset = text(fill, "commissionAsset")?;
        *fees.entry(asset).or_insert(0.0) += number(&fill["commission"])?;
    }
    Ok(OrderResult {
        order_id: body["orderId"]
            .as_i64()
            .ok_or_else(|| DataError::Malformed("missing `orderId`".into()))?,
        client_order_id: text(body, "clientOrderId")?,
        status: text(body, "status")?,
        executed_qty: number(&body["executedQty"])?,
        quote_qty: number(&body["cummulativeQuoteQty"])?,
        fees,
    })
}

#[cfg(test)]
mod tests;
