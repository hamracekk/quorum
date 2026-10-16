//! The UCITS funds quorum may hold, keyed by ISIN (how a broker finds a fund).
//!
//! EU retail can buy only UCITS funds with a KID (DESIGN §19.1). Each fund carries the exchanges,
//! symbols and currencies it lists on, and the US-listed fund that stands in for it in research
//! until the broker's own prices exist.

use std::path::Path;

use quorum_core::InstrumentId;
use quorum_core::calendar::Exchange;
use serde::Deserialize;

/// Where a fund trades.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listing {
    pub exchange: Exchange,
    pub symbol: String,
    /// Trading currency (ISO code).
    pub currency: String,
}

/// One UCITS fund.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fund {
    pub isin: String,
    pub name: String,
    /// What it holds, e.g. `world equity`.
    pub exposure: String,
    /// Reinvests income (no yearly tax until sold in CZ) rather than paying it out.
    pub accumulating: bool,
    /// Ongoing charge as a fraction, when known.
    #[serde(default)]
    pub ter: Option<f64>,
    /// The research stand-in (same index, US-listed), if any.
    #[serde(default)]
    pub stand_in: Option<InstrumentId>,
    /// The listings were checked against the broker's contract search.
    #[serde(default)]
    pub verified: bool,
    pub listings: Vec<Listing>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UcitsUniverse {
    pub funds: Vec<Fund>,
}

impl UcitsUniverse {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let universe: Self = toml::from_str(text).map_err(|e| e.to_string())?;
        for fund in &universe.funds {
            if !valid_isin(&fund.isin) {
                return Err(format!(
                    "{}: `{}` is not a valid ISIN",
                    fund.name, fund.isin
                ));
            }
            if universe
                .funds
                .iter()
                .filter(|f| f.isin == fund.isin)
                .count()
                > 1
            {
                return Err(format!("{}: listed twice", fund.isin));
            }
            if fund.listings.is_empty() {
                return Err(format!("{}: no listings", fund.isin));
            }
            for l in &fund.listings {
                let iso =
                    l.currency.len() == 3 && l.currency.chars().all(|c| c.is_ascii_uppercase());
                if !iso || l.symbol.trim().is_empty() {
                    return Err(format!(
                        "{}: listing `{}` on {}",
                        fund.isin, l.symbol, l.exchange
                    ));
                }
            }
            if fund.ter.is_some_and(|t| !(0.0..0.03).contains(&t)) {
                return Err(format!("{}: ter must be a fraction below 3%", fund.isin));
            }
        }
        Ok(universe)
    }

    #[must_use]
    pub fn fund(&self, isin: &str) -> Option<&Fund> {
        self.funds.iter().find(|f| f.isin == isin)
    }
}

/// Whether `isin` is well formed and its check digit agrees.
///
/// Two letters, nine letters or digits, then the check digit (ISO 6166: letters become 10–35,
/// then the Luhn check).
#[must_use]
pub fn valid_isin(isin: &str) -> bool {
    let bytes = isin.as_bytes();
    if bytes.len() != 12
        || !bytes[..2].iter().all(u8::is_ascii_uppercase)
        || !bytes[2..11]
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        || !bytes[11].is_ascii_digit()
    {
        return false;
    }
    let mut digits: Vec<u32> = Vec::with_capacity(24);
    for &b in bytes {
        let value = if b.is_ascii_digit() {
            u32::from(b - b'0')
        } else {
            u32::from(b - b'A') + 10
        };
        if value >= 10 {
            digits.push(value / 10);
        }
        digits.push(value % 10);
    }
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| {
            if i % 2 == 1 {
                let doubled = d * 2;
                doubled / 10 + doubled % 10
            } else {
                d
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isin_check_digits() {
        assert!(valid_isin("IE00BK5BQT80"));
        assert!(valid_isin("US0378331005"));
        assert!(!valid_isin("IE00BK5BQT81"), "wrong check digit");
        assert!(!valid_isin("IE00BK5BQT8"), "too short");
        assert!(!valid_isin("ie00bk5bqt80"), "lower case");
    }

    #[test]
    fn the_shipped_universe_parses_with_valid_isins() {
        let u = UcitsUniverse::parse(include_str!("../../../config/universe/ucits.toml"))
            .expect("valid universe");
        assert!(u.fund("IE00BK5BQT80").is_some());
        assert!(u.funds.iter().all(|f| valid_isin(&f.isin)));
    }

    #[test]
    fn a_bad_isin_or_currency_is_refused() {
        let base = r#"
[[funds]]
isin = "IE00BK5BQT80"
name = "x"
exposure = "world equity"
accumulating = true
listings = [{ exchange = "xetra", symbol = "VWCE", currency = "EUR" }]
"#;
        assert!(UcitsUniverse::parse(base).is_ok());
        assert!(UcitsUniverse::parse(&base.replace("IE00BK5BQT80", "IE00BK5BQT81")).is_err());
        assert!(UcitsUniverse::parse(&base.replace("\"EUR\"", "\"euro\"")).is_err());
        assert!(UcitsUniverse::parse(&base.replace("\"xetra\"", "\"tokyo\"")).is_err());
    }
}
