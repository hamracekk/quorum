mod buy_and_hold;
mod ensemble;
mod per_instrument;
mod signal_follower;
mod sma_cross;

pub use buy_and_hold::BuyAndHold;
pub use ensemble::{Ensemble, EnsembleConfig, SourceWeight};
pub use per_instrument::PerInstrument;
pub use signal_follower::SignalFollower;
pub use sma_cross::{Sma, SmaCross};
