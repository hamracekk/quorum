use super::*;

const PAPER: &str = r#"
currency = "EUR"
capital_total = 0
leverage_max = 1.0
venues_allowed = ["binance"]
loss = { daily_max = 0.02, total_max = 0.15 }
slot_default = { capital_max = 0, max_mode = "paper" }
"#;

fn with(extra: &str) -> String {
    format!("{PAPER}\n{extra}")
}

fn venue(id: &str) -> VenueId {
    VenueId::new(id).expect("valid venue")
}

#[test]
fn paper_envelope_parses() {
    let env = Envelope::parse(PAPER).expect("valid");
    assert!(env.is_paper_only());
    assert_eq!(env.max_mode("crypto-spot"), Mode::Paper);
    assert!(env.allows_venue(&venue("binance")));
    assert!(!env.allows_venue(&venue("ibkr")));
    assert_eq!(env.currency(), "EUR");
}

#[test]
fn the_shipped_envelope_is_valid() {
    let text = include_str!("../../../config/envelope.toml");
    Envelope::parse(text).expect("config/envelope.toml must be valid");
}

#[test]
fn unknown_keys_are_rejected() {
    let typo = PAPER.replace("max_mode", "max_mod");
    assert!(matches!(
        Envelope::parse(&typo),
        Err(EnvelopeError::Parse(_))
    ));
    assert!(Envelope::parse(&with("extra = 1")).is_err());
}

#[test]
fn live_mode_without_capital_is_rejected() {
    let live = PAPER.replace(r#"max_mode = "paper""#, r#"max_mode = "live-micro""#);
    let err = Envelope::parse(&live).expect_err("no capital");
    assert!(err.to_string().contains("needs capital_max > 0"), "{err}");
}

#[test]
fn slot_cap_cannot_exceed_total() {
    let text = with("[slots.crypto-spot]\ncapital_max = 100\nmax_mode = \"paper\"");
    let err = Envelope::parse(&text).expect_err("cap over total");
    assert!(err.to_string().contains("exceeds capital_total"), "{err}");
}

#[test]
fn loss_limits_are_checked() {
    let daily_over_total = PAPER.replace("daily_max = 0.02", "daily_max = 0.5");
    assert!(Envelope::parse(&daily_over_total).is_err());
    let not_a_fraction = PAPER.replace("total_max = 0.15", "total_max = 15");
    assert!(Envelope::parse(&not_a_fraction).is_err());
}

#[test]
fn per_slot_bounds_override_the_default() {
    let text = PAPER
        .replace("capital_total = 0", "capital_total = 5000")
        .replace(
            "venues_allowed = [\"binance\"]",
            "venues_allowed = [\"binance\", \"ibkr\"]",
        )
        + "\n[slots.equities]\ncapital_max = 2000\nmax_mode = \"live-micro\"\n";
    let env = Envelope::parse(&text).expect("valid");
    assert_eq!(env.max_mode("equities"), Mode::LiveMicro);
    assert_eq!(env.max_mode("crypto-spot"), Mode::Paper);
    assert!(!env.is_paper_only());
    assert!((env.slot("equities").capital_max - 2000.0).abs() < f64::EPSILON);
}

#[test]
fn modes_are_ordered_and_step_one_rung() {
    assert!(Mode::Research < Mode::Paper && Mode::Paper < Mode::LiveMicro);
    assert_eq!(Mode::Paper.next(), Some(Mode::LiveMicro));
    assert_eq!(Mode::Live.next(), None);
    assert_eq!(Mode::Research.prev(), None);
    assert!(!Mode::Paper.is_live() && Mode::LiveMicro.is_live());
    assert_eq!(Mode::LiveMicro.to_string(), "live-micro");
}

#[test]
fn the_longterm_book_has_its_own_bounds() {
    assert_eq!(Envelope::parse(PAPER).expect("valid").longterm(), None);
    let text = with("[longterm]\ncapital_max = 0\nmax_mode = \"paper\"\nposition_max = 0.7");
    let env = Envelope::parse(&text).expect("valid");
    let lt = env.longterm().expect("bounds");
    assert!((lt.position_max - 0.7).abs() < f64::EPSILON);
    assert_eq!(lt.max_mode, Mode::Paper);
    assert!(env.is_paper_only());

    let live = text.replace(
        "max_mode = \"paper\"\nposition_max",
        "max_mode = \"live\"\nposition_max",
    );
    let err = Envelope::parse(&live).expect_err("live without capital");
    assert!(err.to_string().contains("longterm"), "{err}");
    assert!(
        Envelope::parse(&text.replace("0.7", "1.5")).is_err(),
        "over leverage_max"
    );
    let as_slot = with("[slots.longterm]\ncapital_max = 0\nmax_mode = \"paper\"");
    assert!(Envelope::parse(&as_slot).is_err(), "the name is reserved");
}
