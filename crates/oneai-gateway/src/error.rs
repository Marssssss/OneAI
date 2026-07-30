//! Gateway error type.

use thiserror::Error;

/// A gateway operation error.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum GatewayError {
    /// A platform adapter failed (signature verification, send, token fetch).
    #[error("platform '{platform}' error: {message}")]
    Platform { platform: String, message: String },

    /// Channel-directory persistence / lookup failure.
    #[error("channel directory error: {0}")]
    Directory(String),

    /// The inbound event could not be parsed into a [`MessageEvent`].
    #[error("event parse error: {0}")]
    Parse(String),

    /// No adapter registered for the platform named in the inbound event.
    #[error("no platform adapter registered for '{0}'")]
    UnknownPlatform(String),

    /// The runner rejected the turn (no provider, busy, etc.).
    #[error("runner rejected: {0}")]
    Rejected(String),

    /// An IO error from the JSON-backed directory store.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A serde JSON error.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// An HTTP error from an outbound platform API call.
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
}

pub type Result<T> = std::result::Result<T, GatewayError>;
