use super::*;

/// Deterministic noise in [-0.01, 0.01) (a linear congruential generator).
fn noise(seed: u64, len: usize) -> Vec<f64> {
    let mut state = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 33) as f64 / f64::from(1_u32 << 31)).mul_add(0.02, -0.01)
        })
        .collect()
}

fn ts(day: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(day * 86_400, 0).expect("valid timestamp")
}

#[test]
fn pure_noise_overfits_about_half_the_time() {
    let returns: Vec<Vec<f64>> = (0..20).map(|s| noise(s, 1_600)).collect();
    let report = pbo(&returns, 10, 365.0).expect("enough data");
    assert_eq!(report.splits, 252); // C(10, 5)
    assert!(
        (0.25..=0.75).contains(&report.pbo),
        "no skill anywhere: {}",
        report.pbo
    );
    assert!(report.median_is_sharpe > report.median_oos_sharpe);
}

#[test]
fn a_real_edge_is_not_overfit() {
    let mut returns: Vec<Vec<f64>> = (0..20).map(|s| noise(s, 1_600)).collect();
    returns[7].iter_mut().for_each(|r| *r += 0.004);
    let report = pbo(&returns, 10, 365.0).expect("enough data");
    assert!(
        report.pbo < 0.05,
        "the drifting set wins everywhere: {}",
        report.pbo
    );
    assert!(report.prob_oos_loss < 0.05);
    assert!(report.median_oos_sharpe > 1.0);
}

#[test]
fn pbo_needs_two_sets_and_enough_bars() {
    assert!(pbo(&[noise(1, 1_000)], 16, 365.0).is_none());
    assert!(pbo(&[noise(1, 10), noise(2, 10)], 16, 365.0).is_none());
    assert!(pbo(&[noise(1, 100), noise(2, 99)], 4, 365.0).is_none());
}

#[test]
fn cpcv_builds_complete_paths() {
    let mut returns: Vec<Vec<f64>> = (0..5).map(|s| noise(s, 600)).collect();
    returns[3].iter_mut().for_each(|r| *r += 0.004);
    let report = cpcv(&returns, 6, 2, 3, 365.0).expect("enough data");
    assert_eq!(report.splits, 15); // C(6, 2)
    assert_eq!(report.path_sharpes.len(), 5); // C(5, 1)
    assert!(report.path_sharpes.windows(2).all(|w| w[0] <= w[1]));
    assert!((report.share_positive - 1.0).abs() < 1e-12);
    assert!(report.median_sharpe > 1.0);
}

#[test]
fn a_single_set_makes_every_path_its_own_returns() {
    let one = noise(9, 400);
    let report = cpcv(std::slice::from_ref(&one), 4, 1, 0, 365.0).expect("enough data");
    let all: Vec<usize> = (0..one.len()).collect();
    assert_eq!(report.path_sharpes.len(), 1);
    assert!((report.path_sharpes[0] - sharpe(&one, &all, 365.0)).abs() < 1e-12);
}

#[test]
fn cpcv_rejects_bad_splits() {
    let returns = [noise(1, 400)];
    assert!(cpcv(&returns, 4, 0, 0, 365.0).is_none());
    assert!(cpcv(&returns, 4, 4, 0, 365.0).is_none());
    assert!(cpcv(&returns, 17, 2, 0, 365.0).is_none());
    assert!(cpcv(&[noise(1, 5)], 4, 1, 0, 365.0).is_none());
}

#[test]
fn returns_follow_the_shared_timeline() {
    let a = vec![
        EquityPoint {
            ts: ts(0),
            equity: 100.0,
        },
        EquityPoint {
            ts: ts(1),
            equity: 110.0,
        },
        EquityPoint {
            ts: ts(2),
            equity: 99.0,
        },
    ];
    // Missing day 1: keeps 100 there.
    let b = vec![
        EquityPoint {
            ts: ts(0),
            equity: 100.0,
        },
        EquityPoint {
            ts: ts(2),
            equity: 120.0,
        },
    ];
    let (stamps, rows) = returns_matrix(&[a, b]);
    assert_eq!(stamps, vec![ts(1), ts(2)]);
    assert!((rows[0][0] - 0.1).abs() < 1e-12);
    assert!((rows[0][1] + 0.1).abs() < 1e-12);
    assert!(rows[1][0].abs() < 1e-12);
    assert!((rows[1][1] - 0.2).abs() < 1e-12);
}

#[test]
fn daily_stamps_give_365_periods_a_year() {
    let stamps: Vec<DateTime<Utc>> = (0..=730).map(ts).collect();
    assert!((periods_per_year(&stamps) - 365.0).abs() < 0.5);
    assert!(periods_per_year(&stamps[..1]).abs() < 1e-12);
}
