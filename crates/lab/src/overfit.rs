//! Overfitting control (E-4, ADR 0007 step 3): the deflated Sharpe ratio.
//!
//! The best of many tried settings looks good by luck alone. The deflated Sharpe ratio (Bailey &
//! López de Prado) is the probability that the best result's true Sharpe is above zero, after
//! accounting for how many settings were tried and how much their Sharpe ratios varied: the
//! observed Sharpe is compared with the Sharpe the best of `trials` would reach under no skill.
//! Returns are assumed normal (skew 0, kurtosis 3) — fat tails would lower it further.

/// Euler–Mascheroni constant.
const EULER: f64 = 0.577_215_664_901_532_9;

/// `coeffs[0]·xⁿ + … + coeffs[n]` by Horner's rule.
fn horner(coeffs: &[f64], x: f64) -> f64 {
    coeffs.iter().fold(0.0, |acc, c| acc.mul_add(x, *c))
}

/// Standard normal cumulative distribution (Abramowitz–Stegun 7.1.26, error < 1.5e-7).
pub fn normal_cdf(x: f64) -> f64 {
    let t = 1.0 / 0.327_591_1_f64.mul_add(x.abs() / std::f64::consts::SQRT_2, 1.0);
    let poly = t * horner(
        &[
            1.061_405_429,
            -1.453_152_027,
            1.421_413_741,
            -0.284_496_736,
            0.254_829_592,
        ],
        t,
    );
    let erf = poly.mul_add(-(-(x * x) / 2.0).exp(), 1.0);
    if x >= 0.0 {
        0.5 * (1.0 + erf)
    } else {
        0.5 * (1.0 - erf)
    }
}

/// Inverse standard normal (Acklam's rational approximation, relative error < 1.2e-9).
pub fn normal_quantile(p: f64) -> f64 {
    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_69e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239,
    ];
    const B: [f64; 6] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
        1.0,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838,
        -2.549_732_539_343_734,
        4.374_664_141_464_968,
        2.938_163_982_698_783,
    ];
    const D: [f64; 5] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996,
        3.754_408_661_907_416,
        1.0,
    ];
    let p = p.clamp(1e-12, 1.0 - 1e-12);
    let tail = |q: f64| horner(&C, q) / horner(&D, q);
    if p < 0.024_25 {
        tail((-2.0 * p.ln()).sqrt())
    } else if p > 1.0 - 0.024_25 {
        -tail((-2.0 * (1.0 - p).ln()).sqrt())
    } else {
        let q = p - 0.5;
        let r = q * q;
        horner(&A, r) * q / horner(&B, r)
    }
}

/// Probability that the best of `trials` results has a true Sharpe above zero.
///
/// `sharpe` and `sharpes` are annualised; `years` is the length of the record behind them and
/// `periods_per_year` the bars per year (365 for daily bars).
pub fn deflated_sharpe(sharpe: f64, sharpes: &[f64], years: f64, periods_per_year: f64) -> f64 {
    let n = sharpes.len();
    let periods = years * periods_per_year;
    if n == 0 || periods < 2.0 || periods_per_year <= 0.0 {
        return 0.0;
    }
    let per = |s: f64| s / periods_per_year.sqrt();
    let observed = per(sharpe);
    let mean = sharpes.iter().map(|s| per(*s)).sum::<f64>() / n as f64;
    let variance = if n > 1 {
        sharpes
            .iter()
            .map(|s| (per(*s) - mean).powi(2))
            .sum::<f64>()
            / (n as f64 - 1.0)
    } else {
        0.0
    };
    // The Sharpe the best of `n` reaches by luck alone.
    let expected_max = if n > 1 {
        let n = n as f64;
        variance.sqrt()
            * (1.0 - EULER).mul_add(
                normal_quantile(1.0 - 1.0 / n),
                EULER * normal_quantile(1.0 - 1.0 / (n * std::f64::consts::E)),
            )
    } else {
        0.0
    };
    // Normal returns: skew 0, kurtosis 3.
    let spread = (0.5 * observed).mul_add(observed, 1.0).sqrt();
    normal_cdf((observed - expected_max) * (periods - 1.0).sqrt() / spread)
}

#[cfg(test)]
mod tests;
