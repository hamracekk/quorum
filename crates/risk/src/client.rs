//! How slots and the CLI reach the risk process.

use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use crate::RiskError;
use crate::protocol::{OrderCheck, Ping, Request, Response, Verdict};
use crate::state::Halt;

/// The order path's view of the risk process. Calls block: the engine's order path is
/// synchronous, and an answer takes well under a millisecond on the same machine.
pub trait RiskGate: fmt::Debug + Send + Sync {
    /// The verdict on one order. `Err` means no verdict: the caller must treat the order as
    /// not approved unless it only reduces risk.
    fn check(&self, order: &OrderCheck) -> Result<Verdict, RiskError>;

    /// Reports the slot alive with its equity; returns the halt that applies to it.
    fn ping(&self, ping: &Ping) -> Result<Option<Halt>, RiskError>;
}

/// The risk process over its Unix socket (`<data>/risk/risk.sock`).
#[derive(Debug, Clone)]
pub struct SocketGate {
    path: PathBuf,
    timeout: Duration,
}

impl SocketGate {
    /// How long a request may take before the process counts as unreachable.
    pub const TIMEOUT: Duration = Duration::from_secs(2);

    #[must_use]
    pub const fn new(path: PathBuf) -> Self {
        Self {
            path,
            timeout: Self::TIMEOUT,
        }
    }

    /// Sends one request and waits for its answer.
    pub fn request(&self, request: &Request) -> Result<Response, RiskError> {
        let unreachable =
            |err: std::io::Error| RiskError::Unreachable(format!("{}: {err}", self.path.display()));
        let mut stream = UnixStream::connect(&self.path).map_err(unreachable)?;
        stream
            .set_read_timeout(Some(self.timeout))
            .map_err(unreachable)?;
        stream
            .set_write_timeout(Some(self.timeout))
            .map_err(unreachable)?;
        let mut line = serde_json::to_string(request)
            .map_err(|e| RiskError::Protocol(format!("request not encoded: {e}")))?;
        line.push('\n');
        stream.write_all(line.as_bytes()).map_err(unreachable)?;
        let mut answer = String::new();
        BufReader::new(&stream)
            .read_line(&mut answer)
            .map_err(unreachable)?;
        if answer.is_empty() {
            return Err(RiskError::Unreachable(format!(
                "{}: closed without an answer",
                self.path.display()
            )));
        }
        let response: Response = serde_json::from_str(answer.trim())
            .map_err(|e| RiskError::Protocol(format!("answer not understood: {e}")))?;
        match response {
            Response::Error { message } => Err(RiskError::Protocol(message)),
            other => Ok(other),
        }
    }
}

impl RiskGate for SocketGate {
    fn check(&self, order: &OrderCheck) -> Result<Verdict, RiskError> {
        match self.request(&Request::Check(order.clone()))? {
            Response::Verdict(verdict) => Ok(verdict),
            other => Err(unexpected(&other)),
        }
    }

    fn ping(&self, ping: &Ping) -> Result<Option<Halt>, RiskError> {
        match self.request(&Request::Ping(ping.clone()))? {
            Response::Pong { halt } => Ok(halt),
            other => Err(unexpected(&other)),
        }
    }
}

fn unexpected(response: &Response) -> RiskError {
    RiskError::Protocol(format!("unexpected answer: {response:?}"))
}
