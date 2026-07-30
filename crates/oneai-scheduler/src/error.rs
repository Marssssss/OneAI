//! Scheduler error type.

use thiserror::Error;

/// A cron-scheduler operation error.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CronError {
    /// A schedule string could not be parsed (`"30m"` / `"every 2h"` / ISO / cron).
    #[error("invalid schedule '{input}': {message}")]
    InvalidSchedule { input: String, message: String },

    /// The job store failed (IO, serialization, CAS conflict).
    #[error("job store error: {0}")]
    Store(String),

    /// A job id was not found.
    #[error("job not found: {0}")]
    NotFound(String),

    /// The delivery seam rejected the job (no provider, no bound channel, busy).
    #[error("delivery rejected: {0}")]
    Rejected(String),

    /// An IO error from the file-backed store.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A serde JSON error.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, CronError>;
