#[derive(Debug, thiserror::Error)]
pub enum LabError {
    #[error("parameter grid is empty")]
    EmptyGrid,
    #[error("train and test windows must both be at least one bar")]
    EmptyWindow,
    #[error("not enough data: need at least {needed} bars, have {available}")]
    NotEnoughData { needed: usize, available: usize },
}
