//! Core trading primitives: market data types, the strategy interface, a simulated
//! broker, risk limits, and the backtest engine that ties them together.
//!
//! The same `Strategy` trait is meant to drive both the backtester and (later) a
//! live/paper executor, so a strategy that backtests is the strategy that trades.

pub mod backtest;
pub mod broker;
pub mod metrics;
pub mod risk;
pub mod strategies;
pub mod strategy;
pub mod types;

pub use backtest::{BacktestConfig, BacktestResult, run, run_from};
pub use metrics::{Performance, Report};
pub use strategy::Strategy;
pub use types::{Bar, EquityPoint, Fill, Series, Side};
