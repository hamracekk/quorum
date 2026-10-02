//! Core trading primitives: market data types, the strategy interface, a simulated
//! broker, risk limits, and the backtest engine that ties them together.
//!
//! The same `Strategy` trait is meant to drive both the backtester and (later) a
//! live/paper executor, so a strategy that backtests is the strategy that trades.

pub mod agents;
pub mod backtest;
pub mod broker;
pub mod engine;
pub mod events;
pub mod fakes;
pub mod indicators;
pub mod instrument;
pub mod metrics;
pub mod news;
pub mod order;
pub mod patterns;
pub mod ports;
pub mod predictions;
pub mod risk;
pub mod signals;
pub mod strategies;
pub mod strategy;
pub mod types;

pub use backtest::{BacktestConfig, BacktestResult, run, run_from, run_with_events};
pub use broker::{Broker, PaperBroker, Portfolio};
pub use engine::{Engine, EngineError, EngineSnapshot, PositionSnapshot};
pub use events::{Envelope, Event};
pub use instrument::{InstrumentError, InstrumentId, InstrumentKind, VenueId};
pub use metrics::{Performance, Report};
pub use order::{ExecutionReport, Order, OrderBook, OrderError, OrderId, OrderStatus};
pub use ports::{
    AgentCall, AgentStore, BarStore, EventLog, Journal, LlmProvider, LlmRequest, LlmResponse,
    MarketData, MarketStream, NewsSource, NewsStore, Notification, Notifier, PortError,
    PredictionSource, RunInfo, RunKind, Severity, StreamedBar,
};
pub use strategy::Strategy;
pub use types::{Bar, EquityPoint, Fill, Series, Side};
