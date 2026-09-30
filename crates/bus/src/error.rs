use std::path::PathBuf;

/// Detailed event-log errors; wrapped in `PortError` at the port boundary.
#[derive(Debug, thiserror::Error)]
pub enum BusError {
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}:{line}: invalid event: {source}")]
    Corrupt {
        path: PathBuf,
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not serialize event: {0}")]
    Serialize(#[source] serde_json::Error),
}
