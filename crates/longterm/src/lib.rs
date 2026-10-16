//! The long-term book (DESIGN §20): buy-and-hold investing kept apart from the trading slots.
//!
//! - [`policy`]: the operator's policy (target weights, monthly contribution, rebalancing band)
//!   and [`PolicyStrategy`], which follows it on the one engine: new money to the holdings
//!   furthest below target, a full rebalance only on a band breach.
//! - [`ledger`]: tax lots in CZK, FIFO, the three-year time test.
//! - [`sim`]: a policy over history in CZK with contributions, money- and time-weighted;
//!   [`drive`] is the same run for the paper book (`runner::longterm`).
//! - [`review`]: the AI review's facts, what a proposal would cost (trading and tax), its score
//!   against the unchanged book, and the journal of reviews and decisions.
//!
//! Prices: US ETFs standing in for UCITS funds (same indices) until IBKR supplies the funds.

pub mod ledger;
pub mod policy;
pub mod review;
pub mod sim;

pub use ledger::{IfSold, Ledger, Lot, Realized};
pub use policy::{Holding, LongTermConfig, Policy, PolicyStrategy, plan};
pub use review::{ReviewConfig, ReviewJournal, ReviewRecord};
pub use sim::{
    Driven, SimCosts, SimReport, drive, engine_config, instruments, report, simulate, xirr,
};

#[derive(Debug, thiserror::Error)]
pub enum LongTermError {
    #[error("long-term config: {0}")]
    Config(String),
    #[error("long-term data: {0}")]
    Data(String),
}
