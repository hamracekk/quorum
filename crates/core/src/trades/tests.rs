use chrono::Duration;

use super::*;

fn day(n: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(0, 0).expect("ts") + Duration::days(n)
}

fn btc() -> InstrumentId {
    InstrumentId::spot("binance", "BTCUSDT").expect("instrument")
}

fn fill(n: i64, side: Side, qty: f64, price: f64) -> Fill {
    Fill {
        ts: day(n),
        instrument: btc(),
        side,
        qty,
        price,
        fee: 1.0,
    }
}

fn decision(n: i64, strategy: &str, target: f64, reason: Option<serde_json::Value>) -> Decision {
    Decision {
        ts: day(n),
        instrument: btc(),
        strategy: strategy.into(),
        target,
        reason,
    }
}

#[test]
fn a_round_trip_has_its_entry_exit_profit_and_causes() {
    let fills = [
        fill(1, Side::Buy, 2.0, 100.0),
        fill(5, Side::Sell, 2.0, 110.0),
    ];
    let decisions = [
        decision(0, "trend-momentum", 0.2, None),
        decision(4, "trend-momentum", 0.0, None),
    ];
    let trips = round_trips(&fills, &decisions, &BTreeMap::new(), day(9));
    assert_eq!(trips.len(), 1);
    let t = &trips[0];
    assert_eq!(t.closed, Some(day(5)));
    assert!(
        (t.pnl - 18.0).abs() < 1e-9,
        "20 gained, 2 in fees: {}",
        t.pnl
    );
    assert!((t.return_pct - 0.09).abs() < 1e-9);
    assert!((t.days_held - 4.0).abs() < 1e-9);
    assert_eq!(t.exit_price, Some(110.0));
    assert_eq!(t.opened_by, "trend-momentum");
    assert_eq!(t.exit_reason.as_deref(), Some("signal"));
}

#[test]
fn stops_exit_on_their_own_bar_and_are_named() {
    let fills = [
        fill(1, Side::Buy, 1.0, 100.0),
        fill(3, Side::Sell, 1.0, 95.0),
    ];
    let decisions = [
        decision(0, "suggestion", 0.1, None),
        // The strategy's later target on day 3 executes on day 4, not this fill.
        decision(3, "trend-momentum", 0.1, None),
        decision(3, "stops", 0.0, Some(serde_json::json!({"exit": "stop"}))),
    ];
    let trips = round_trips(&fills, &decisions, &BTreeMap::new(), day(9));
    assert_eq!(trips[0].opened_by, "manual");
    assert_eq!(trips[0].exit_reason.as_deref(), Some("stop"));
}

#[test]
fn a_close_the_strategy_did_not_ask_for_is_the_risk_layer() {
    let fills = [
        fill(1, Side::Buy, 1.0, 100.0),
        fill(3, Side::Sell, 1.0, 90.0),
    ];
    let decisions = [decision(0, "trend-momentum", 0.1, None)];
    let trips = round_trips(&fills, &decisions, &BTreeMap::new(), day(9));
    assert_eq!(trips[0].exit_reason.as_deref(), Some("risk"));
}

#[test]
fn adds_and_trims_stay_one_trade_with_a_weighted_entry() {
    let fills = [
        fill(1, Side::Buy, 1.0, 100.0),
        fill(2, Side::Buy, 1.0, 120.0),
        fill(3, Side::Sell, 1.0, 130.0),
        fill(4, Side::Sell, 1.0, 100.0),
    ];
    let trips = round_trips(&fills, &[], &BTreeMap::new(), day(9));
    assert_eq!(trips.len(), 1);
    let t = &trips[0];
    assert!((t.entry_price - 110.0).abs() < 1e-9);
    assert!((t.max_qty - 2.0).abs() < 1e-9);
    assert_eq!(t.exit_price, Some(115.0));
    // +20 then −10, four fees.
    assert!((t.pnl - 6.0).abs() < 1e-9, "{}", t.pnl);
    assert_eq!(t.opened_by, "unknown");
}

#[test]
fn a_flip_closes_the_trade_and_opens_the_other_side() {
    let fills = [
        fill(1, Side::Buy, 1.0, 100.0),
        fill(2, Side::Sell, 3.0, 110.0),
    ];
    let trips = round_trips(&fills, &[], &BTreeMap::new(), day(9));
    assert_eq!(trips.len(), 2);
    assert!(
        (trips[0].pnl - (10.0 - 1.0 - 1.0 / 3.0)).abs() < 1e-9,
        "a third of the fee"
    );
    assert!((trips[1].direction + 1.0).abs() < 1e-12, "short");
    assert!((trips[1].max_qty - 2.0).abs() < 1e-12);
}

#[test]
fn open_trades_are_valued_at_the_mark() {
    let fills = [fill(1, Side::Buy, 2.0, 100.0)];
    let marks = BTreeMap::from([(btc(), 105.0)]);
    let trips = round_trips(&fills, &[], &marks, day(3));
    let t = &trips[0];
    assert_eq!(t.closed, None);
    assert_eq!(t.mark, Some(105.0));
    assert!((t.pnl - 9.0).abs() < 1e-9);
    assert!((t.days_held - 2.0).abs() < 1e-9);
    assert_eq!(stats(&trips).open, 1);
}

#[test]
fn stats_summarise_closed_trades() {
    let fills = [
        fill(1, Side::Buy, 1.0, 100.0),
        fill(2, Side::Sell, 1.0, 121.0),
        fill(3, Side::Buy, 1.0, 100.0),
        fill(5, Side::Sell, 1.0, 91.0),
    ];
    let s = stats(&round_trips(&fills, &[], &BTreeMap::new(), day(9)));
    assert_eq!((s.closed, s.wins, s.losses), (2, 1, 1));
    assert!((s.win_rate - 0.5).abs() < 1e-12);
    assert!((s.profit_factor - 19.0 / 11.0).abs() < 1e-9);
    assert!((s.expectancy - 4.0).abs() < 1e-9);
    assert!((s.avg_days_held - 1.5).abs() < 1e-9);
    assert!((s.best - 19.0).abs() < 1e-9 && (s.worst + 11.0).abs() < 1e-9);
    assert_eq!(s.by_exit.get("unknown"), Some(&2));
}

#[test]
fn no_trades_give_empty_stats() {
    let s = stats(&[]);
    assert_eq!(s, TradeStats::default());
    assert!(round_trips(&[], &[], &BTreeMap::new(), day(0)).is_empty());
}
