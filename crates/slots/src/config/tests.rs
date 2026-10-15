use super::*;

const SLOT: &str = r#"
id = "crypto-spot"
venue = "binance"
interval = "1d"
strategies = ["trend-momentum"]

[universe]
kind = "top-volume"
size = 25
window = 60
refresh = 30
min_history = 365

[costs]
fee_rate = 0.001
slippage_bps = 2.0
"#;

const PAPER_ENVELOPE: &str = r#"
currency = "EUR"
capital_total = 0
leverage_max = 1.0
venues_allowed = ["binance"]
loss = { daily_max = 0.02, total_max = 0.15 }
slot_default = { capital_max = 0, max_mode = "paper" }
"#;

fn err(text: &str) -> String {
    SlotConfig::parse(text).expect_err("invalid").to_string()
}

#[test]
fn parses_with_defaults() {
    let slot = SlotConfig::parse(SLOT).expect("valid");
    assert_eq!(slot.id.as_str(), "crypto-spot");
    assert_eq!(slot.start_mode, Mode::Research);
    assert_eq!(slot.calendar, TradingCalendar::AlwaysOpen);
    assert!(slot.costs.impact.abs() < f64::EPSILON);
    let rule = slot.universe.top_volume_rule().expect("top volume");
    assert_eq!(
        (rule.size, rule.window, rule.refresh, rule.min_history),
        (25, 60, 30, 365)
    );
}

#[test]
fn the_shipped_slot_is_valid_inside_the_shipped_envelope() {
    let slot = SlotConfig::parse(include_str!("../../../../config/slots/crypto-spot.toml"))
        .expect("config/slots/crypto-spot.toml must be valid");
    let envelope = Envelope::parse(include_str!("../../../../config/envelope.toml"))
        .expect("config/envelope.toml must be valid");
    slot.check_envelope(&envelope).expect("venue allowed");
}

#[test]
fn costs_apply_to_a_backtest_config() {
    let slot =
        SlotConfig::parse(&SLOT.replace("slippage_bps = 2.0", "slippage_bps = 5.0\nimpact = 0.05"))
            .expect("valid");
    let mut cfg = BacktestConfig::default();
    slot.costs.apply(&mut cfg);
    assert!((cfg.slippage_bps - 5.0).abs() < f64::EPSILON);
    assert!((cfg.impact - 0.05).abs() < f64::EPSILON);
}

#[test]
fn unknown_keys_are_rejected() {
    assert!(err(&SLOT.replace("fee_rate", "fee")).contains("fee"));
    assert!(SlotConfig::parse(&format!("leverage = 3\n{SLOT}")).is_err());
}

#[test]
fn live_start_modes_are_rejected() {
    let live = format!("start_mode = \"live-micro\"\n{SLOT}");
    assert!(err(&live).contains("only through the ladder"));
}

#[test]
fn invalid_values_are_rejected() {
    assert!(err(&SLOT.replace(r#"interval = "1d""#, r#"interval = "5m""#)).contains("interval"));
    assert!(err(&SLOT.replace(r#"["trend-momentum"]"#, "[]")).contains("strategy"));
    assert!(err(&SLOT.replace(r#"["trend-momentum"]"#, r#"["a", "a"]"#)).contains("twice"));
    assert!(err(&SLOT.replace("size = 25", "size = 0")).contains("> 0"));
    assert!(err(&SLOT.replace("fee_rate = 0.001", "fee_rate = -0.001")).contains("costs"));
    assert!(SlotConfig::parse(&SLOT.replace("crypto-spot", "Crypto Spot")).is_err());
}

#[test]
fn fixed_universe_must_stay_on_the_slot_venue() {
    let fixed = SLOT.replace(
        "kind = \"top-volume\"\nsize = 25\nwindow = 60\nrefresh = 30\nmin_history = 365",
        "kind = \"fixed\"\ninstruments = [\"binance:BTCUSDT\", \"kraken:XBTUSD\"]",
    );
    assert!(err(&fixed).contains("not on venue binance"));
}

#[test]
fn venues_outside_the_envelope_are_rejected() {
    let envelope = Envelope::parse(PAPER_ENVELOPE).expect("valid");
    let slot = SlotConfig::parse(&SLOT.replace(r#"venue = "binance""#, r#"venue = "ibkr""#))
        .expect("valid on its own");
    let message = slot
        .check_envelope(&envelope)
        .expect_err("ibkr not allowed")
        .to_string();
    assert!(message.contains("venues_allowed"), "{message}");
}
