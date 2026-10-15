use chrono::{Duration, TimeZone, Utc};

use super::*;

fn bars(closes: &[f64]) -> Vec<Bar> {
    let t0 = Utc
        .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
        .single()
        .expect("valid");
    closes
        .iter()
        .enumerate()
        .map(|(i, &c)| Bar {
            ts: t0 + Duration::days(i64::try_from(i).expect("small")),
            open: c,
            high: c,
            low: c,
            close: c,
            volume: 100.0,
        })
        .collect()
}

#[test]
fn momentum_is_the_lookback_return() {
    let b = bars(&[100.0, 50.0, 110.0]);
    let m = Momentum { lookback: 2 }.value(&b).expect("value");
    assert!((m - 0.1).abs() < 1e-12);
    assert!(
        Momentum { lookback: 3 }.value(&b).is_none(),
        "warm-up not reached"
    );
}

#[test]
fn values_use_only_bars_up_to_the_last() {
    // Point in time: appending a future bar must not change the value at the earlier bar.
    let b = bars(&[100.0, 101.0, 99.0, 104.0, 107.0, 103.0]);
    for factor in price_family(365.0) {
        let warm = factor.warmup().min(4);
        let before = factor.value(&b[..=warm]);
        let again = factor.value(&b[..=warm]);
        assert_eq!(
            before.map(f64::to_bits),
            again.map(f64::to_bits),
            "{}",
            factor.id()
        );
    }
}

#[test]
fn trend_strength_prefers_a_steady_rise() {
    let steady = bars(&[100.0, 101.0, 102.0, 103.0, 104.0]);
    let jumpy = bars(&[100.0, 108.0, 96.0, 110.0, 104.0]);
    let t = TrendStrength { lookback: 4 };
    assert!(t.value(&steady).expect("steady") > t.value(&jumpy).expect("jumpy"));
}

#[test]
fn flat_or_broken_prices_give_no_value() {
    let flat = bars(&[100.0; 10]);
    assert!(
        RealizedVol {
            window: 5,
            periods_per_year: 365.0
        }
        .value(&flat)
        .is_none()
    );
    assert!(MeanReversionZ { window: 5 }.value(&flat).is_none());
    let broken = bars(&[100.0, 0.0, 100.0]);
    assert!(Momentum { lookback: 2 }.value(&broken).is_none());
}

#[test]
fn volume_surprise_compares_the_last_bar_with_the_window_before_it() {
    let mut b = bars(&[100.0; 5]);
    b[4].volume = 400.0;
    let v = VolumeSurprise { window: 4 }.value(&b).expect("value");
    assert!((v - 4.0_f64.ln()).abs() < 1e-12);
}

#[test]
fn ranks_average_ties_and_scale_to_unit_range() {
    assert_eq!(ranks(&[3.0, 1.0, 3.0, 2.0]), vec![2.5, 0.0, 2.5, 1.0]);
    let ids: Vec<InstrumentId> = ["A", "B", "C"]
        .iter()
        .map(|s| InstrumentId::spot("binance", s).expect("valid"))
        .collect();
    let scores = rank_scores(&[
        (ids[0].clone(), 5.0),
        (ids[1].clone(), -1.0),
        (ids[2].clone(), 2.0),
    ]);
    let by: Vec<f64> = scores.iter().map(|(_, s)| *s).collect();
    assert_eq!(by, vec![1.0, -1.0, 0.0]);
    assert!(rank_scores(&scores[..1]).is_empty());
}

#[test]
fn factor_ids_are_versioned() {
    let ids: Vec<String> = price_family(365.0)
        .iter()
        .map(|f| f.id().to_string())
        .collect();
    assert!(ids.contains(&"momentum_60@1".to_owned()));
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "every factor has its own id");
}
