//! Venue-qualified instrument identity (ADR 0001).
//!
//! `BTCUSDT` on Binance and `BTC` perpetual on Hyperliquid are different instruments, so
//! every instrument carries its venue and kind. The text form is `venue:SYMBOL` for spot
//! and `venue:SYMBOL:kind` otherwise, e.g. `binance:BTCUSDT`, `hyperliquid:BTC:perp`.
//!
//! Venue ids and symbols are validated to a conservative character set because they end
//! up in file paths and config keys.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InstrumentError {
    #[error("invalid venue id `{0}`: use lowercase letters, digits, `-` or `_`")]
    Venue(String),
    #[error("invalid symbol `{0}`: use letters, digits, `.`, `-` or `_`")]
    Symbol(String),
    #[error("unknown instrument kind `{0}` (spot, perp, future, equity, prediction)")]
    Kind(String),
    #[error("invalid instrument `{0}`: expected `venue:SYMBOL` or `venue:SYMBOL:kind`")]
    Format(String),
}

/// Identifier of a trading venue, e.g. `binance`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct VenueId(String);

impl VenueId {
    pub fn new(id: impl Into<String>) -> Result<Self, InstrumentError> {
        let id = id.into();
        let valid = !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if valid {
            Ok(Self(id))
        } else {
            Err(InstrumentError::Venue(id))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for VenueId {
    type Error = InstrumentError;

    fn try_from(id: String) -> Result<Self, Self::Error> {
        Self::new(id)
    }
}

impl From<VenueId> for String {
    fn from(venue: VenueId) -> Self {
        venue.0
    }
}

impl fmt::Display for VenueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum InstrumentKind {
    #[default]
    Spot,
    Perpetual,
    Future,
    Equity,
    PredictionMarket,
}

impl InstrumentKind {
    /// Short code used in the text form and in file names.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Spot => "spot",
            Self::Perpetual => "perp",
            Self::Future => "future",
            Self::Equity => "equity",
            Self::PredictionMarket => "prediction",
        }
    }

    /// Whether the market trades on Saturdays and Sundays (UTC). Stock exchanges do not, so a
    /// missing weekend bar there is the calendar, not a sign the instrument stopped trading.
    pub const fn trades_weekends(self) -> bool {
        !matches!(self, Self::Equity)
    }
}

impl FromStr for InstrumentKind {
    type Err = InstrumentError;

    fn from_str(code: &str) -> Result<Self, Self::Err> {
        match code {
            "spot" => Ok(Self::Spot),
            "perp" | "perpetual" => Ok(Self::Perpetual),
            "future" => Ok(Self::Future),
            "equity" => Ok(Self::Equity),
            "prediction" => Ok(Self::PredictionMarket),
            other => Err(InstrumentError::Kind(other.to_owned())),
        }
    }
}

/// A tradable instrument on a specific venue.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct InstrumentId {
    venue: VenueId,
    symbol: String,
    kind: InstrumentKind,
}

impl InstrumentId {
    /// `symbol` is the venue-native symbol, e.g. `BTCUSDT` on Binance.
    pub fn new(
        venue: VenueId,
        symbol: impl Into<String>,
        kind: InstrumentKind,
    ) -> Result<Self, InstrumentError> {
        let symbol = symbol.into();
        let valid = !symbol.is_empty()
            && symbol
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
        if !valid {
            return Err(InstrumentError::Symbol(symbol));
        }
        Ok(Self {
            venue,
            symbol,
            kind,
        })
    }

    pub fn spot(venue: &str, symbol: &str) -> Result<Self, InstrumentError> {
        Self::new(VenueId::new(venue)?, symbol, InstrumentKind::Spot)
    }

    /// Parse `venue:SYMBOL[:kind]`; a bare `SYMBOL` is spot on `default_venue`.
    pub fn parse_with_default(
        text: &str,
        default_venue: &VenueId,
    ) -> Result<Self, InstrumentError> {
        if text.contains(':') {
            text.parse()
        } else {
            Self::new(default_venue.clone(), text, InstrumentKind::Spot)
        }
    }

    pub const fn venue(&self) -> &VenueId {
        &self.venue
    }

    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    pub const fn kind(&self) -> InstrumentKind {
        self.kind
    }
}

impl fmt::Display for InstrumentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            InstrumentKind::Spot => write!(f, "{}:{}", self.venue, self.symbol),
            kind => write!(f, "{}:{}:{}", self.venue, self.symbol, kind.code()),
        }
    }
}

impl FromStr for InstrumentId {
    type Err = InstrumentError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = text.split(':').collect();
        match parts.as_slice() {
            [venue, symbol] => Self::new(VenueId::new(*venue)?, *symbol, InstrumentKind::Spot),
            [venue, symbol, kind] => Self::new(VenueId::new(*venue)?, *symbol, kind.parse()?),
            _ => Err(InstrumentError::Format(text.to_owned())),
        }
    }
}

impl TryFrom<String> for InstrumentId {
    type Error = InstrumentError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        text.parse()
    }
}

impl From<InstrumentId> for String {
    fn from(instrument: InstrumentId) -> Self {
        instrument.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spot_round_trips_through_text() {
        let id: InstrumentId = "binance:BTCUSDT".parse().expect("valid");
        assert_eq!(id.venue().as_str(), "binance");
        assert_eq!(id.symbol(), "BTCUSDT");
        assert_eq!(id.kind(), InstrumentKind::Spot);
        assert_eq!(id.to_string(), "binance:BTCUSDT");
    }

    #[test]
    fn non_spot_kinds_carry_a_suffix() {
        let id: InstrumentId = "hyperliquid:BTC:perp".parse().expect("valid");
        assert_eq!(id.kind(), InstrumentKind::Perpetual);
        assert_eq!(id.to_string(), "hyperliquid:BTC:perp");
    }

    #[test]
    fn bare_symbol_uses_default_venue() {
        let venue = VenueId::new("binance").expect("valid");
        let id = InstrumentId::parse_with_default("ETHUSDT", &venue).expect("valid");
        assert_eq!(id.to_string(), "binance:ETHUSDT");
    }

    #[test]
    fn rejects_path_like_symbols_and_bad_venues() {
        assert!(matches!(
            InstrumentId::spot("binance", "../etc"),
            Err(InstrumentError::Symbol(_))
        ));
        assert!(matches!(
            InstrumentId::spot("Binance", "BTCUSDT"),
            Err(InstrumentError::Venue(_))
        ));
        assert!(matches!(
            "a:b:c:d".parse::<InstrumentId>(),
            Err(InstrumentError::Format(_))
        ));
        assert!(matches!(
            "binance:BTC:swap".parse::<InstrumentId>(),
            Err(InstrumentError::Kind(_))
        ));
    }

    #[test]
    fn serializes_as_its_text_form() {
        let id: InstrumentId = "binance:BTCUSDT".parse().expect("valid");
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, "\"binance:BTCUSDT\"");
        let back: InstrumentId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id);
        assert!(serde_json::from_str::<InstrumentId>("\"../x\"").is_err());
    }
}
