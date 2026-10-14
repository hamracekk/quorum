use chrono::{DateTime, Utc};
use trader_core::{
    BacktestConfig, BacktestResult, Bar, Engine, EngineError, Event, EventLog, InstrumentId,
    PortError, Strategy,
};

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("reading the event log: {0}")]
    Log(#[from] PortError),
    #[error(transparent)]
    Engine(#[from] EngineError),
}

/// Replay the market bars recorded in `[start, end)` through `strategy`, using the same
/// [`Engine`] a backtest uses.
///
/// Recorded decisions (signals, fills, …) are ignored: they are *re-derived*, which is what
/// makes replay a test of determinism. The benchmark universe is every instrument seen, in
/// order of first appearance.
pub fn replay(
    log: &dyn EventLog,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    strategy: &mut dyn Strategy,
    cfg: &BacktestConfig,
) -> Result<(BacktestResult, Vec<Event>), ReplayError> {
    let bars: Vec<(InstrumentId, Bar)> = log
        .read(start, end)?
        .into_iter()
        .filter_map(|envelope| match envelope.event {
            Event::Bar { instrument, bar } => Some((instrument, bar)),
            _ => None,
        })
        .collect();

    let mut universe: Vec<InstrumentId> = Vec::new();
    for (instrument, _) in &bars {
        if !universe.contains(instrument) {
            universe.push(instrument.clone());
        }
    }

    let mut engine = Engine::new(strategy, cfg.clone(), &universe, None);
    let mut events = Vec::new();
    for (instrument, bar) in &bars {
        events.extend(engine.on_bar(instrument, bar)?);
    }
    let (result, tail) = engine.finish();
    events.extend(tail);
    Ok((result, events))
}

#[cfg(test)]
mod tests {
    use trader_core::fakes::MemoryEventLog;
    use trader_core::strategies::BuyAndHold;

    use super::*;

    fn at(hours: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(hours * 3_600, 0).expect("valid timestamp")
    }

    fn bar_event(hour: i64, price: f64) -> Event {
        Event::Bar {
            instrument: InstrumentId::spot("test", "A").expect("valid instrument"),
            bar: Bar {
                ts: at(hour),
                open: price,
                high: price,
                low: price,
                close: price,
                volume: 1.0,
            },
        }
    }

    #[test]
    fn recorded_decisions_are_ignored_and_re_derived() {
        let log = MemoryEventLog::default();
        log.append(&bar_event(0, 100.0)).expect("append");
        log.append(&Event::RiskHalt {
            ts: at(0),
            equity: 1.0,
        })
        .expect("append");
        log.append(&bar_event(1, 100.0)).expect("append");

        let (result, events) = replay(
            &log,
            at(0),
            at(10),
            &mut BuyAndHold::default(),
            &BacktestConfig::default(),
        )
        .expect("replay");
        assert_eq!(result.fills.len(), 1);
        assert!(events.iter().all(|e| e.kind() != "risk_halt"));
    }

    #[test]
    fn bars_recorded_out_of_time_order_are_rejected() {
        let log = MemoryEventLog::default();
        log.append(&bar_event(2, 100.0)).expect("append");
        log.append(&bar_event(1, 100.0)).expect("append");
        let err = replay(
            &log,
            at(0),
            at(10),
            &mut BuyAndHold::default(),
            &BacktestConfig::default(),
        )
        .expect_err("out of order");
        assert!(matches!(err, ReplayError::Engine(_)));
    }
}
