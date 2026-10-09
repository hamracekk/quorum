use super::*;

#[test]
fn the_normal_helpers_match_known_values() {
    assert!((normal_cdf(0.0) - 0.5).abs() < 1e-7);
    assert!((normal_cdf(1.96) - 0.975).abs() < 1e-3);
    assert!((normal_quantile(0.975) - 1.959_964).abs() < 1e-5);
    assert!((normal_quantile(0.5)).abs() < 1e-9);
}

#[test]
fn many_trials_deflate_the_same_sharpe() {
    let one = deflated_sharpe(1.0, &[1.0], 4.0, 365.0);
    assert!(one > 0.95, "one honest try over 4 years: {one}");
    let tried: Vec<f64> = (0..100)
        .map(|i| f64::from(i).mul_add(0.015, -0.5))
        .collect();
    let best_of_many = deflated_sharpe(1.0, &tried, 4.0, 365.0);
    assert!(best_of_many < one, "{best_of_many} vs {one}");
    assert!(
        best_of_many < 0.5,
        "below what luck alone gives the best of 100: {best_of_many}"
    );
    // Above the luck threshold, a longer record makes the result more certain.
    let long = deflated_sharpe(2.0, &tried, 4.0, 365.0);
    let short = deflated_sharpe(2.0, &tried, 0.5, 365.0);
    assert!(long > 0.9 && short < long, "{short} vs {long}");
}
