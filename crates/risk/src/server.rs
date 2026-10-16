//! The risk process: a Unix socket server that owns the risk state and answers every request.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use quorum_core::{Notification, Notifier, Severity};
use quorum_envelope::Envelope;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::RiskError;
use crate::protocol::{Request, Response, StatusReport};
use crate::rules::{self, Transition};
use crate::state::{Halt, RiskState};

/// Most bytes one connection may send: requests are a few hundred bytes.
const MAX_REQUEST: u64 = 64 * 1024;

/// Where the risk process keeps its files: `<data>/risk/`, plus the operator's envelope.
#[derive(Debug, Clone)]
pub struct RiskPaths {
    pub dir: PathBuf,
    pub envelope: PathBuf,
}

impl RiskPaths {
    #[must_use]
    pub fn new(data: &Path, envelope: &Path) -> Self {
        Self {
            dir: data.join("risk"),
            envelope: envelope.to_path_buf(),
        }
    }

    /// The socket slots and the CLI connect to.
    #[must_use]
    pub fn socket(&self) -> PathBuf {
        self.dir.join("risk.sock")
    }

    #[must_use]
    pub fn state(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    /// Held by the one running risk process.
    #[must_use]
    pub fn lock(&self) -> PathBuf {
        self.dir.join("LOCK")
    }
}

#[derive(Debug)]
struct Shared {
    state: Mutex<RiskState>,
    paths: RiskPaths,
    notifier: Arc<dyn Notifier>,
}

/// Runs the risk process until `shutdown` resolves. One process per data directory: a second
/// one fails on the lock instead of taking over the socket.
pub async fn serve(
    paths: RiskPaths,
    notifier: Arc<dyn Notifier>,
    shutdown: impl Future<Output = ()> + Send,
) -> Result<(), RiskError> {
    let io = |path: &Path| {
        let path = path.display().to_string();
        move |source| RiskError::Io { path, source }
    };
    std::fs::create_dir_all(&paths.dir).map_err(io(&paths.dir))?;
    let lock_path = paths.lock();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&lock_path)
        .map_err(io(&lock_path))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(RiskError::Locked(lock_path.display().to_string()));
        }
        Err(std::fs::TryLockError::Error(source)) => return Err(io(&lock_path)(source)),
    }
    let socket = paths.socket();
    // Holding the lock, any socket file left is from a process that died.
    match std::fs::remove_file(&socket) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            return Err(io(&socket)(err));
        }
        _ => {}
    }
    let listener = UnixListener::bind(&socket).map_err(io(&socket))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .map_err(io(&socket))?;
    }
    let state = RiskState::load(&paths.state())?;
    if Envelope::load(&paths.envelope).is_err() {
        tracing::warn!(envelope = %paths.envelope.display(), "envelope unreadable: only risk-reducing orders pass");
    }
    tracing::info!(socket = %socket.display(), kill = ?state.kill, "risk process listening");
    let shared = Arc::new(Shared {
        state: Mutex::new(state),
        paths,
        notifier,
    });
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let shared = Arc::clone(&shared);
                    tokio::spawn(async move {
                        if let Err(err) = connection(&shared, stream).await {
                            tracing::warn!(error = %err, "risk connection failed");
                        }
                    });
                }
                Err(err) => tracing::warn!(error = %err, "risk accept failed"),
            },
        }
    }
    drop(listener);
    let _ = std::fs::remove_file(&socket);
    tracing::info!("risk process stopped");
    Ok(())
}

async fn connection(shared: &Shared, stream: UnixStream) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read.take(MAX_REQUEST));
    let mut line = String::new();
    while lines.read_line(&mut line).await? > 0 {
        let response = match serde_json::from_str::<Request>(line.trim()) {
            Ok(request) => handle(shared, request).await,
            Err(err) => Response::Error {
                message: format!("not a risk request: {err}"),
            },
        };
        let mut out = serde_json::to_string(&response).map_err(std::io::Error::other)?;
        out.push('\n');
        write.write_all(out.as_bytes()).await?;
        line.clear();
    }
    Ok(())
}

async fn handle(shared: &Shared, request: Request) -> Response {
    let envelope = match Envelope::load(&shared.paths.envelope) {
        Ok(envelope) => Some(envelope),
        Err(err) => {
            tracing::error!(error = %err, "envelope unreadable");
            None
        }
    };
    let now = Utc::now();
    let mut state = shared.state.lock().await;
    let (response, transitions) = match request {
        Request::Check(order) => {
            let (verdict, transitions) = rules::check(&mut state, envelope.as_ref(), &order, now);
            if !verdict.approved {
                tracing::info!(slot = %order.slot, instrument = %order.instrument, reason = %verdict.reason, "order rejected");
            }
            (Response::Verdict(verdict), transitions)
        }
        Request::Ping(ping) => {
            let transitions = envelope.as_ref().map_or_else(Vec::new, |e| {
                rules::observe(&mut state, &ping.slot, ping.equity, e.loss(), now)
            });
            let halt = state.halt_for(&ping.slot).cloned();
            (Response::Pong { halt }, transitions)
        }
        Request::Halt { slot, reason } => {
            let transition = rules::halt(&mut state, slot.as_deref(), &reason);
            (Response::Done, vec![transition])
        }
        Request::Resume { slot } => {
            let transitions = rules::resume(&mut state, slot.as_deref());
            (Response::Done, transitions)
        }
        Request::Status => (
            Response::Status(StatusReport {
                kill: state.kill.clone(),
                envelope_ok: envelope.is_some(),
                slots: state
                    .slots
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            }),
            Vec::new(),
        ),
    };
    if let Err(err) = state.save(&shared.paths.state()) {
        tracing::error!(error = %err, "risk state not saved");
    }
    drop(state);
    for transition in transitions {
        alert(shared.notifier.as_ref(), &transition).await;
    }
    response
}

/// Best effort: a failed alert is logged, never fails the request.
async fn alert(notifier: &dyn Notifier, transition: &Transition) {
    let (severity, what, slot, halt) = match transition {
        Transition::Halted { slot, halt } => {
            let severity = match halt {
                Halt::Daily { .. } => Severity::Warning,
                Halt::Total | Halt::Operator { .. } => Severity::Critical,
            };
            (severity, "halted", slot, halt)
        }
        Transition::Lifted { slot, halt } => (Severity::Info, "resumed", slot, halt),
    };
    let scope = slot.as_deref().unwrap_or("every slot");
    tracing::warn!(scope, what, halt = %halt.describe(), "risk transition");
    let body = match transition {
        Transition::Halted { .. } => format!(
            "{} · no new risk; risk-reducing orders still pass",
            halt.describe()
        ),
        Transition::Lifted { .. } => format!("lifted: {}", halt.describe()),
    };
    let note = Notification {
        ts: Utc::now(),
        severity,
        title: format!("risk · {scope} {what}"),
        body,
    };
    if let Err(err) = notifier.notify(&note).await {
        tracing::warn!(error = %err, "notification not delivered");
    }
}
