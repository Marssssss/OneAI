//! Newline-delimited JSON wire codec for the sidecar / IPC framing.
//!
//! Each frame is one [`Directive`] or [`EngineYield`] serialized as a single
//! line terminated by `\n` — the same framing `oneai-supervisor` already uses
//! over UDS / named-pipe (so the sidecar can reuse its `IpcListener` /
//! `IpcStream`). Bidirectional: the server emits yields (encode
//! [`EngineYield`]) and ingests directives (decode [`Directive`]); the client
//! does the reverse.
//!
//! Approval correlation is carried by the `request_id` field inside the
//! `approval_request` / `approve` payloads — no envelope wrapping needed.

use serde::{de::DeserializeOwned, Serialize};

use crate::protocol::{Directive, EngineYield};
use crate::{BusError, Result};

/// Serialize a directive as one JSON line terminated by `\n`.
pub fn serialize_directive(d: &Directive) -> Result<String> {
    encode(d)
}

/// Serialize a yield as one JSON line terminated by `\n`.
pub fn serialize_yield(y: &EngineYield) -> Result<String> {
    encode(y)
}

/// Parse a single directive line (no trailing newline required).
pub fn parse_directive(line: &str) -> Result<Directive> {
    decode(line.trim())
}

/// Parse a single yield line (no trailing newline required).
pub fn parse_yield(line: &str) -> Result<EngineYield> {
    decode(line.trim())
}

fn encode<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value)
        .map(|mut s| {
            s.push('\n');
            s
        })
        .map_err(|e| BusError::Codec(e.to_string()))
}

fn decode<T: DeserializeOwned>(line: &str) -> Result<T> {
    serde_json::from_str(line).map_err(|e| BusError::Codec(e.to_string()))
}
