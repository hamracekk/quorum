//! Slots (DESIGN §5.2–5.3): one market on one venue — its universe, strategies, calendar and
//! costs — and its rung on the autonomy ladder.
//!
//! Not to be confused with `quorum_core::strategies::Slots`, a *strategy* that holds the best
//! N instruments of a universe. A slot here is the unit the allocator funds and the ladder
//! promotes; such a strategy can run inside one.
//!
//! - [`SlotConfig`]: the operator's description of a slot (`config/slots/<id>.toml`).
//! - [`SlotState`]: where the slot stands (mode, since when, halted), and
//!   [`SlotState::review`], which promotes or demotes it from measured [`Evidence`] — never
//!   above the envelope's `max_mode` for the slot.

mod config;
mod ladder;
pub mod ucits;

pub use config::{
    Brakes, CostModel, Exits, Sizing, SlotConfig, SlotConfigError, SlotId, StrategySpec,
    TradingCalendar, Universe, VolRegimeBrake,
};
pub use ladder::{
    Alarms, Blocker, Evidence, Gates, LiveRecord, PaperRecord, ResearchRecord, SlotState,
    Transition, Trigger,
};
pub use quorum_envelope::Mode;
