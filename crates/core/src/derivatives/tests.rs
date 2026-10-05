use chrono::Duration;

use super::*;

fn points(rates: &[f64]) -> Vec<FundingPoint> {
    let start = DateTime::from_timestamp(0, 0).expect("ts");
    rates
        .iter()
        .zip(0_i64..)
        .map(|(&rate, i)| FundingPoint {
            ts: start + Duration::hours(8 * i),
            rate,
        })
        .collect()
}

#[test]
fn an_extreme_z_score_calls_against_the_crowd_once_per_spell() {
    let mut rates: Vec<f64> = (0..20)
        .map(|i| if i % 2 == 0 { 0.0001 } else { 0.00012 })
        .collect();
    rates.extend([0.002, 0.003, 0.0001, 0.0001]);
    let rule = FundingRule {
        window: 20,
        ..FundingRule::default()
    };
    let calls = z_calls(&points(&rates), &rule);
    assert_eq!(
        calls.len(),
        1,
        "two extreme prints in a row are one call: {calls:?}"
    );
    assert!(
        (calls[0].direction + 1.0).abs() < f64::EPSILON,
        "crowded longs: bearish"
    );
    assert!((calls[0].rate - 0.002).abs() < f64::EPSILON);
}

#[test]
fn the_level_rule_calls_when_funding_enters_a_band() {
    let rates = [0.0001, 0.0006, 0.0007, 0.0001, -0.0002, -0.0003, 0.0006];
    let calls = level_calls(&points(&rates), &FundingRule::default());
    let directions: Vec<f64> = calls.iter().map(|c| c.direction).collect();
    assert_eq!(directions, [-1.0, 1.0, -1.0]);
    assert!((annualised(0.0001, 3.0) - 0.1095).abs() < 1e-12);
}
