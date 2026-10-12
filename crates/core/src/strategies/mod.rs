mod buy_and_hold;
mod ensemble;
mod follow_traders;
mod momentum;
mod per_instrument;
mod ridge;
mod signal_follower;
mod sma_cross;
mod universe;

pub use buy_and_hold::BuyAndHold;
pub use ensemble::{Ensemble, EnsembleConfig, SourceWeight};
pub use follow_traders::{FollowTraders, PREFIX as FOLLOW_PREFIX};
pub use momentum::{TrendMomentum, VolTarget, XsMomentum};
pub use per_instrument::PerInstrument;
pub use ridge::XsRidge;
pub use signal_follower::SignalFollower;
pub use sma_cross::{Sma, SmaCross};
pub use universe::{TopVolume, UniverseRule};
