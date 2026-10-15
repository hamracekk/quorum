use chrono::TimeZone;

use super::*;

fn book() -> (Vec<Level>, Vec<Level>) {
    // Mid 100. Bids 99.95, 99.6, 99.2, 98.5; asks 100.05, 100.4, 100.8, 101.5, 102.5.
    let bids = vec![(99.95, 1.0), (99.6, 2.0), (99.2, 3.0), (98.5, 4.0)];
    let asks = vec![
        (100.05, 1.0),
        (100.4, 2.0),
        (100.8, 3.0),
        (101.5, 4.0),
        (102.5, 1.0),
    ];
    (bids, asks)
}

fn notional(levels: &[Level]) -> f64 {
    levels.iter().map(|&(p, q)| p * q).sum()
}

#[test]
fn spread_and_mid_come_from_the_best_levels() {
    let (bids, asks) = book();
    let s = summarize_book(&bids, &asks).expect("valid book");
    assert!((s.mid - 100.0).abs() < 1e-9);
    assert!((s.spread_bps - 10.0).abs() < 1e-9, "{}", s.spread_bps);
    assert_eq!(s.levels, 4);
}

#[test]
fn depth_bands_sum_the_notional_inside_each_band() {
    let (bids, asks) = book();
    let s = summarize_book(&bids, &asks).expect("valid book");
    let at = |bands: &[DepthBand], pct: f64| {
        bands
            .iter()
            .find(|b| (b.pct - pct).abs() < 1e-12)
            .expect("band")
            .notional
    };
    // Within 0.5% of 100 on the bid side: 99.95 and 99.6.
    let half = at(&s.bids, 0.5).expect("covered");
    assert!((half - notional(&[(99.95, 1.0), (99.6, 2.0)])).abs() < 1e-9);
    // Within 1% on the ask side: 100.05, 100.4, 100.8.
    let one = at(&s.asks, 1.0).expect("covered");
    assert!((one - notional(&[(100.05, 1.0), (100.4, 2.0), (100.8, 3.0)])).abs() < 1e-9);
}

#[test]
fn bands_beyond_the_snapshot_are_unknown_not_zero() {
    let (bids, asks) = book();
    let s = summarize_book(&bids, &asks).expect("valid book");
    // Bids reach 98.5 (−1.5%): the 2% band is not covered.
    assert_eq!(s.bids[3].notional, None);
    // Asks reach 102.5 (+2.5%): covered.
    assert!(s.asks[3].notional.is_some());
}

#[test]
fn imbalance_compares_one_percent_depth() {
    let (bids, asks) = book();
    let s = summarize_book(&bids, &asks).expect("valid book");
    let imbalance = s.imbalance_1pct.expect("both sides cover 1%");
    assert!(imbalance < 0.0, "more ask depth within 1%: {imbalance}");
}

#[test]
fn empty_or_crossed_books_have_no_summary() {
    assert!(summarize_book(&[], &[(1.0, 1.0)]).is_none());
    assert!(summarize_book(&[(101.0, 1.0)], &[(100.0, 1.0)]).is_none());
}

#[test]
fn records_serialize_with_their_kind_and_feed() {
    let ts = Utc
        .with_ymd_and_hms(2026, 10, 15, 12, 0, 0)
        .single()
        .expect("valid");
    let marker = Record::Marker(Marker {
        known_time: ts,
        feed: Feed::Liquidations,
        state: MarkerState::Down,
        detail: "connection reset".into(),
    });
    let json = serde_json::to_string(&marker).expect("serialize");
    assert!(json.starts_with(r#"{"kind":"marker""#), "{json}");
    assert!(json.contains(r#""state":"down""#));
    let back: Record = serde_json::from_str(&json).expect("round trip");
    assert_eq!(back, marker);
    assert_eq!(back.feed(), Feed::Liquidations);
    assert_eq!(back.known_time(), ts);
}
