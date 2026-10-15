use chrono::Duration;

use super::*;

fn id(i: usize) -> InstrumentId {
    InstrumentId::spot("test", &format!("C{i:02}")).expect("valid instrument")
}

const PLAIN: VolTarget = VolTarget {
    target_vol: 0.0,
    vol_bars: 10,
    periods_per_year: 365.0,
    band: 0.0,
};

fn config(slots: usize, fill_always: bool) -> SlotsConfig {
    SlotsConfig {
        slots,
        lookback: 20,
        rebalance: 1,
        fill_always,
        conviction_weight: 1.0,
        sizing: PLAIN,
    }
}

fn evidence() -> EnsembleConfig {
    EnsembleConfig {
        technical: false,
        ..EnsembleConfig::default()
    }
}

/// `coins` daily series; coin i drifts by `drift(i, day)` a day with a small wobble.
fn feed(
    s: &mut Slots,
    coins: usize,
    days: i64,
    drift: impl Fn(usize, i64) -> f64,
) -> BTreeMap<InstrumentId, f64> {
    let start = DateTime::<Utc>::from_timestamp(0, 0).expect("ts");
    let mut price = vec![100.0_f64; coins];
    let mut last = BTreeMap::new();
    for day in 0..days {
        for (i, p) in price.iter_mut().enumerate() {
            let wobble = if (day + i64::try_from(i).expect("small")) % 3 == 0 {
                1.003
            } else {
                0.999
            };
            *p *= (1.0 + drift(i, day)) * wobble;
            let bar = Bar {
                ts: start + Duration::days(day),
                open: *p,
                high: *p,
                low: *p,
                close: *p,
                volume: 1.0,
            };
            if let Some(t) = s.on_bar(&id(i), &bar) {
                last.insert(id(i), t);
            }
        }
    }
    last
}

fn held(targets: &BTreeMap<InstrumentId, f64>) -> Vec<InstrumentId> {
    targets
        .iter()
        .filter(|(_, t)| **t > 0.0)
        .map(|(i, _)| i.clone())
        .collect()
}

#[test]
fn every_slot_is_filled_with_the_strongest_trends() {
    // 30 coins: coin i drifts by (i − 15) basis points a day, so the top 20 are coins 10…29.
    let mut s = Slots::new(config(20, true), evidence(), Vec::new());
    let last = feed(&mut s, 30, 80, |i, _| (i as f64 - 15.0) * 0.0001);
    let chosen = held(&last);
    assert_eq!(chosen.len(), 20, "{chosen:?}");
    assert!(chosen.iter().all(|c| c.symbol() >= "C10"), "{chosen:?}");
    assert!(
        (last[&id(29)] - 0.05).abs() < 1e-12,
        "an equal 1/20 share each"
    );
}

#[test]
fn positive_only_leaves_slots_empty_rather_than_hold_falling_coins() {
    let mut s = Slots::new(config(20, false), evidence(), Vec::new());
    let last = feed(&mut s, 30, 80, |i, _| (i as f64 - 15.0) * 0.0001);
    let chosen = held(&last);
    // The wobble adds ~+3 bp a day, so coins 12 and up trend upwards: 18 of them, not 20.
    assert_eq!(chosen.len(), 18, "{chosen:?}");
    assert!(chosen.iter().all(|c| c.symbol() >= "C12"), "{chosen:?}");
}

#[test]
fn a_holding_keeps_its_slot_while_it_ranks_within_the_buffer() {
    // 3 slots, buffer 1: coin 2 starts among the top 3, then slips to 4th — it stays.
    let mut s = Slots::new(config(3, true), evidence(), Vec::new());
    let last = feed(&mut s, 6, 120, |i, day| {
        let rank = if day < 60 {
            [1.0, 2.0, 5.0, 4.0, 3.0, 0.0]
        } else {
            [1.0, 2.0, 3.6, 4.0, 3.8, 0.0]
        };
        rank[i] * 0.001
    });
    let chosen = held(&last);
    assert!(
        chosen.contains(&id(2)),
        "kept within the buffer: {chosen:?}"
    );
    assert_eq!(chosen.len(), 3);
}

#[test]
fn without_a_record_the_sources_add_nothing() {
    let mut s = Slots::new(config(5, true), evidence(), Vec::new());
    feed(&mut s, 10, 60, |i, _| i as f64 * 0.0005);
    let why = s.explain(&id(9)).expect("explained");
    assert!(
        why["conviction"].as_f64().is_some_and(|c| c.abs() < 1e-12),
        "{why}"
    );
    assert_eq!(why["in_slot"], true);
    assert!(why["trend"].as_f64().is_some_and(|t| t > 0.0), "{why}");
}

#[test]
fn later_bars_cannot_change_earlier_targets() {
    let record = |days: i64| {
        let mut s = Slots::new(config(4, true), evidence(), Vec::new());
        let start = DateTime::<Utc>::from_timestamp(0, 0).expect("ts");
        let mut out = Vec::new();
        let mut price = [100.0_f64; 8];
        for day in 0..days {
            for (i, p) in price.iter_mut().enumerate() {
                let k = i64::try_from(i).expect("small");
                *p *= (((day * 7 + k * 3) % 11 - 5) as f64).mul_add(0.002, 1.0);
                let bar = Bar {
                    ts: start + Duration::days(day),
                    open: *p,
                    high: *p,
                    low: *p,
                    close: *p,
                    volume: 1.0,
                };
                out.push(s.on_bar(&id(i), &bar));
            }
        }
        out
    };
    let short = record(60);
    let long = record(90);
    assert_eq!(short[..], long[..short.len()]);
}
