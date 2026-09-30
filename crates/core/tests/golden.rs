//! Golden backtest: a fixed synthetic market through a fixed strategy must produce exactly
//! the same fills and equity, bit for bit. Refactors of execution (broker, orders, engine)
//! must not change results; if this fails, the change altered trading behaviour.

use chrono::{DateTime, Utc};
use trader_core::risk::RiskLimits;
use trader_core::strategies::{PerInstrument, SmaCross};
use trader_core::{BacktestConfig, Bar, InstrumentId, Series, run};

/// Deterministic pseudo-random walk (64-bit LCG), so the test needs no data files.
fn walk(symbol: &str, seed: u64, bars: i64) -> Series {
    let mut state = seed;
    let mut price = 100.0_f64;
    let bars = (0..bars)
        .map(|h| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let step = ((state >> 11) as f64 / (1_u64 << 53) as f64 - 0.5) * 0.04;
            let open = price;
            price *= 1.0 + step;
            Bar {
                ts: DateTime::<Utc>::from_timestamp(h * 3_600, 0).expect("valid timestamp"),
                open,
                high: open.max(price) * 1.002,
                low: open.min(price) * 0.998,
                close: price,
                volume: 1.0,
            }
        })
        .collect();
    Series::new(
        InstrumentId::spot("test", symbol).expect("valid instrument"),
        bars,
    )
}

#[test]
fn execution_is_bit_for_bit_stable() {
    let series = [walk("AAA", 7, 2_000), walk("BBB", 11, 2_000)];
    let cfg = BacktestConfig {
        risk: RiskLimits {
            allow_short: true,
            ..RiskLimits::default()
        },
        ..BacktestConfig::default()
    };
    let mut strategy = PerInstrument::new(SmaCross::new(5, 20, true), series.len());
    let result = run(&mut strategy, &series, &cfg);

    let fingerprint = result
        .fills
        .iter()
        .flat_map(|f| [f.qty, f.price, f.fee])
        .chain(result.equity_curve.iter().map(|p| p.equity))
        .fold(0_u64, |acc, x| {
            acc.rotate_left(7) ^ x.to_bits().wrapping_mul(0x9E37_79B9_7F4A_7C15)
        });
    let last = result.equity_curve.last().expect("equity recorded").equity;
    assert_eq!(result.fills.len(), 278);
    assert_eq!(
        last.to_bits(),
        4_664_902_390_175_924_346,
        "final equity {last}"
    );
    assert_eq!(fingerprint, 0x9a13_8e3d_ad52_99c7);
}
