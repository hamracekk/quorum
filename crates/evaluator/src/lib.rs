//! The trade evaluator (DESIGN §10), slice 1: **shadow**. It builds a case file for every entry
//! a strategy proposes and labels each with how it turned out, taken or not (counterfactual
//! tracking, §10.3). Nothing here changes a trade; the size multiplier stays 1.0 until a meta
//! model beats "always take" out of sample.
//!
//! - [`case`]: [`cases`] from the engine's target events and the bars they were made on.
//! - [`study`](mod@study): what "always take" earns per horizon and regime, and which case
//!   features tell winners from losers out of sample, corrected for the number tested.

pub mod case;
pub mod study;

pub use case::{Case, CaseConfig, EXIT, HORIZONS, cases};
pub use study::{EvaluatorStudy, RegimeSkip, SkipVerdict, StudyConfig, Verdict, study};
