use chrono::Duration;

use super::*;
use crate::strategies::{BuyAndHold, PerInstrument};

fn id(symbol: &str) -> InstrumentId {
    InstrumentId::spot("test", symbol).expect("instrument")
}

fn bar(day: i64, volume: f64) -> Bar {
    Bar {
        ts: DateTime::from_timestamp(0, 0).expect("ts") + Duration::days(day),
        open: 10.0,
        high: 10.0,
        low: 10.0,
        close: 10.0,
        volume,
    }
}

#[test]
fn only_the_most_traded_instruments_at_the_time_are_held() {
    let rule = UniverseRule {
        size: 1,
        window: 3,
        refresh: 1,
        min_history: 2,
    };
    let mut s = TopVolume::new(Box::new(PerInstrument::new(BuyAndHold::default(), 1)), rule);
    let mut last: BTreeMap<String, f64> = BTreeMap::new();
    // BIG trades more for 10 days, then SMALL takes over.
    for day in 0..20 {
        let (big, small) = if day < 10 {
            (1_000.0, 10.0)
        } else {
            (10.0, 1_000.0)
        };
        for (symbol, volume) in [("BIG", big), ("SMALL", small)] {
            if let Some(t) = s.on_bar(&id(symbol), &bar(day, volume)) {
                last.insert(symbol.to_owned(), t);
            }
        }
        if day == 8 {
            assert_eq!(last.get("BIG"), Some(&1.0), "{last:?}");
            assert_eq!(last.get("SMALL"), Some(&0.0));
        }
    }
    assert_eq!(
        last.get("SMALL"),
        Some(&1.0),
        "the new leader joins: {last:?}"
    );
    assert_eq!(last.get("BIG"), Some(&0.0), "the old one leaves");
    assert!(s.members().contains(&id("SMALL")));
}
