//! A scripted [`RiskGate`] for tests of the order path.

use std::sync::{Mutex, PoisonError};

use crate::RiskError;
use crate::client::RiskGate;
use crate::protocol::{OrderCheck, Ping, Verdict};
use crate::state::Halt;

/// How a [`FakeGate`] answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeMode {
    /// Approves every order.
    Allow,
    /// Refuses every order that does not reduce risk, with this reason.
    RefuseIncreases(String),
    /// Unreachable: every call fails.
    Down,
    /// The slot is halted: increases are refused and pings report the halt.
    Halted(Halt),
}

/// Answers as its [`FakeMode`] says and remembers every order it was asked about.
#[derive(Debug)]
pub struct FakeGate {
    mode: Mutex<FakeMode>,
    checks: Mutex<Vec<OrderCheck>>,
}

impl FakeGate {
    #[must_use]
    pub const fn new(mode: FakeMode) -> Self {
        Self {
            mode: Mutex::new(mode),
            checks: Mutex::new(Vec::new()),
        }
    }

    pub fn set(&self, mode: FakeMode) {
        *self.mode.lock().unwrap_or_else(PoisonError::into_inner) = mode;
    }

    /// Every order asked about so far.
    #[must_use]
    pub fn checks(&self) -> Vec<OrderCheck> {
        self.checks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn mode(&self) -> FakeMode {
        self.mode
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl RiskGate for FakeGate {
    fn check(&self, order: &OrderCheck) -> Result<Verdict, RiskError> {
        self.checks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(order.clone());
        match self.mode() {
            FakeMode::Allow => Ok(Verdict::approve("fake: allow")),
            FakeMode::RefuseIncreases(reason) if !order.reduces_risk() => {
                Ok(Verdict::reject(reason))
            }
            FakeMode::Halted(halt) if !order.reduces_risk() => Ok(Verdict::reject(halt.describe())),
            FakeMode::RefuseIncreases(_) | FakeMode::Halted(_) => {
                Ok(Verdict::approve("reduces risk"))
            }
            FakeMode::Down => Err(RiskError::Unreachable("fake: down".to_owned())),
        }
    }

    fn ping(&self, _ping: &Ping) -> Result<Option<Halt>, RiskError> {
        match self.mode() {
            FakeMode::Down => Err(RiskError::Unreachable("fake: down".to_owned())),
            FakeMode::Halted(halt) => Ok(Some(halt)),
            _ => Ok(None),
        }
    }
}
