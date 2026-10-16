//! Tax lots in CZK, by the Czech rules (to be confirmed with an advisor).
//!
//! Every purchase is a lot with its date and its cost in CZK at the ČNB rate of the day; sales
//! take the oldest lots first (FIFO). A lot held more than three years is exempt (the time test);
//! the rest of each year's gains, net of that year's losses, is taxed at 15%.

use std::collections::{BTreeMap, VecDeque};

use chrono::{DateTime, Datelike, Months, Utc};
use quorum_core::{Fill, InstrumentId, Side};
use serde::{Deserialize, Serialize};

/// Years a lot must be held to be tax-free.
pub const TIME_TEST_YEARS: u32 = 3;

/// One purchase still (partly) held.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Lot {
    pub qty: f64,
    pub bought: DateTime<Utc>,
    /// What it cost per unit in CZK, the purchase fee included.
    pub cost_czk: f64,
}

impl Lot {
    /// When this lot becomes tax-free.
    #[must_use]
    pub fn exempt_from(&self) -> DateTime<Utc> {
        self.bought + Months::new(12 * TIME_TEST_YEARS)
    }
}

/// A sale's gain on one lot.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Realized {
    pub ts: DateTime<Utc>,
    pub instrument: InstrumentId,
    pub gain_czk: f64,
    /// Held over three years: tax-free.
    pub exempt: bool,
}

/// Every lot held and every gain realised.
#[derive(Debug, Clone, Default)]
pub struct Ledger {
    lots: BTreeMap<InstrumentId, VecDeque<Lot>>,
    realized: Vec<Realized>,
}

impl Ledger {
    /// Books a fill; `czk_per_usd` is the ČNB rate on its day (fills are in USD).
    pub fn apply(&mut self, fill: &Fill, czk_per_usd: f64) {
        if fill.qty <= 0.0 {
            return;
        }
        let lots = self.lots.entry(fill.instrument.clone()).or_default();
        match fill.side {
            Side::Buy => lots.push_back(Lot {
                qty: fill.qty,
                bought: fill.ts,
                cost_czk: (fill.price + fill.fee / fill.qty) * czk_per_usd,
            }),
            Side::Sell => {
                let proceeds = (fill.price - fill.fee / fill.qty) * czk_per_usd;
                let mut left = fill.qty;
                // Each pass consumes a whole lot or the rest of the sale.
                for _ in 0..=lots.len() {
                    if left <= 1e-12 {
                        break;
                    }
                    let Some(lot) = lots.front_mut() else {
                        break; // selling more than held: not a long-term book
                    };
                    let take = left.min(lot.qty);
                    self.realized.push(Realized {
                        ts: fill.ts,
                        instrument: fill.instrument.clone(),
                        gain_czk: (proceeds - lot.cost_czk) * take,
                        exempt: fill.ts >= lot.exempt_from(),
                    });
                    lot.qty -= take;
                    left -= take;
                    if lot.qty <= 1e-12 {
                        lots.pop_front();
                    }
                }
            }
        }
    }

    #[must_use]
    pub fn realized(&self) -> &[Realized] {
        &self.realized
    }

    pub fn lots(&self) -> impl Iterator<Item = (&InstrumentId, &Lot)> {
        self.lots
            .iter()
            .flat_map(|(id, lots)| lots.iter().map(move |l| (id, l)))
    }

    /// Tax per calendar year on realised gains: `rate` of the year's non-exempt gains net of its
    /// non-exempt losses (nothing in a losing year, no carry-forward).
    #[must_use]
    pub fn tax_by_year(&self, rate: f64) -> BTreeMap<i32, f64> {
        let mut net: BTreeMap<i32, f64> = BTreeMap::new();
        for r in self.realized.iter().filter(|r| !r.exempt) {
            *net.entry(r.ts.year()).or_default() += r.gain_czk;
        }
        net.into_iter()
            .map(|(y, g)| (y, g.max(0.0) * rate))
            .collect()
    }

    /// What selling every lot at `prices` (USD) on `now` would mean for tax.
    #[must_use]
    pub fn if_sold(
        &self,
        prices: &BTreeMap<InstrumentId, f64>,
        czk_per_usd: f64,
        now: DateTime<Utc>,
    ) -> IfSold {
        let mut out = IfSold::default();
        for (id, lot) in self.lots() {
            let Some(price) = prices.get(id) else {
                continue;
            };
            let value = price * czk_per_usd * lot.qty;
            let gain = lot.cost_czk.mul_add(-lot.qty, value);
            if now >= lot.exempt_from() {
                out.exempt_gain += gain;
            } else {
                out.taxable_gain += gain;
                if lot.exempt_from() <= now + Months::new(12) {
                    out.exempt_within_year += value;
                }
            }
        }
        out
    }
}

/// What selling the whole book now would mean for tax.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
/// All amounts in CZK.
pub struct IfSold {
    pub taxable_gain: f64,
    pub exempt_gain: f64,
    /// Value of the lots that become tax-free within the next year.
    pub exempt_within_year: f64,
}
