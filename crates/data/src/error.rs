use std::path::PathBuf;

/// Detailed adapter errors; wrapped in `PortError` at the port boundary.
#[derive(Debug, thiserror::Error)]
pub enum DataError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("unexpected exchange response: {0}")]
    Malformed(String),
    /// Something the operator has to set up first (e.g. an API key), with how.
    #[error("{0}")]
    Setup(String),
    /// The venue answered, but its answer leaves the outcome unknown (an order may or may not
    /// exist): look it up before retrying.
    #[error("outcome unknown: {0}")]
    Uncertain(String),
    #[error("invalid interval `{0}`: use letters and digits only, e.g. 1h")]
    InvalidInterval(String),
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parquet error: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),
    #[error("arrow error: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),
    #[error("{path} is invalid: {reason}")]
    Corrupt { path: PathBuf, reason: String },
}

pub type Result<T> = std::result::Result<T, DataError>;
