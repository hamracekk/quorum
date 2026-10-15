//! Machine-learned signal sources (M9).
//!
//! Models here are [`quorum_core::strategies::Learner`]s: they plug into the cross-sectional
//! scaffold ([`quorum_core::strategies::XsLearner`]), which trains them only on snapshots whose
//! outcome was already known, so they are judged walk-forward, out of sample and after costs like
//! every strategy — against the linear benchmark ([`quorum_core::strategies::XsRidge`]). No I/O,
//! no randomness: the same bars give the same trees.

pub mod gbdt;

pub use gbdt::{BoostParams, Gbdt, XsBoost};
