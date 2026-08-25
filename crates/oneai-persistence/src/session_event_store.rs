//! File-backed session event store — per-session append-only JSONL log of bus
//! yield events (issue #40 trajectory replay).
//!
//! ## Storage layout
//! - `<root>/events/{session_id}.jsonl` — one serialized `EngineYield` per
//!   line (whitelist-filtered by the producer; this store is format-agnostic
//!   and only guarantees JSON-line integrity).
//!
//! ## Crash safety
//! Append-only: a partial final line fails JSON validation and is skipped on
//! load (same policy as `FileWorkingStateStore`). The session id is
//! sanitized to a safe file stem so an adversarial/foreign id cannot escape
//! the events directory.

use std::path::PathBuf;

use async_trait::async_trait;
use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::SessionEventStore;
use tokio::io::AsyncWriteExt;

/// File-backed session event store.
pub struct FileSessionEventStore {
    root: PathBuf,
}

impl FileSessionEventStore {
    /// Create a store rooted at `root`; logs live under `<root>/events/`.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The JSONL path for one session. The id is sanitized to
    /// `[A-Za-z0-9_-]` so no id can produce path traversal.
    fn path_for(&self, session_id: &str) -> PathBuf {
        let safe: String = session_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let safe = if safe.is_empty() {
            "_unknown".to_string()
        } else {
            safe
        };
        self.root.join("events").join(format!("{safe}.jsonl"))
    }
}

#[async_trait]
impl SessionEventStore for FileSessionEventStore {
    async fn append(&self, session_id: &str, line: &str) -> Result<()> {
        let path = self.path_for(session_id);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| OneAIError::Persistence(format!("create events dir: {e}")))?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| OneAIError::Persistence(format!("open event log: {e}")))?;
        file.write_all(line.as_bytes())
            .await
            .map_err(|e| OneAIError::Persistence(format!("append event: {e}")))?;
        file.write_all(b"\n")
            .await
            .map_err(|e| OneAIError::Persistence(format!("append event newline: {e}")))?;
        file.flush()
            .await
            .map_err(|e| OneAIError::Persistence(format!("flush event log: {e}")))?;
        Ok(())
    }

    async fn load(&self, session_id: &str) -> Result<Vec<String>> {
        let path = self.path_for(session_id);
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(OneAIError::Persistence(format!(
                    "read event log {}: {e}",
                    path.display()
                )))
            }
        };
        let mut out = Vec::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // Validate JSON integrity; a torn final line (crash mid-write)
            // is skipped rather than failing the whole replay.
            if serde_json::from_str::<serde_json::Value>(line).is_ok() {
                out.push(line.to_string());
            } else {
                tracing::warn!(
                    session = session_id,
                    "skipping corrupt session-event line ({} bytes)",
                    line.len()
                );
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn append_then_load_round_trips_in_order() {
        let tmp = TempDir::new().unwrap();
        let store = FileSessionEventStore::new(tmp.path().to_path_buf());
        store
            .append("s1", r#"{"kind":"turn_start","turn_id":"t1","task":"hi"}"#)
            .await
            .unwrap();
        store
            .append("s1", r#"{"kind":"turn_complete","turn_id":"t1"}"#)
            .await
            .unwrap();
        // A different session stays isolated.
        store
            .append("s2", r#"{"kind":"turn_start"}"#)
            .await
            .unwrap();

        let lines = store.load("s1").await.unwrap();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("t1"));
        assert!(lines[1].contains("turn_complete"));
        assert_eq!(store.load("s2").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn load_missing_session_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let store = FileSessionEventStore::new(tmp.path().to_path_buf());
        assert!(store.load("never-existed").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn corrupt_line_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let store = FileSessionEventStore::new(tmp.path().to_path_buf());
        store
            .append("s1", r#"{"kind":"turn_start"}"#)
            .await
            .unwrap();
        // Simulate a torn write: a non-JSON fragment appended raw.
        let path = store.path_for("s1");
        tokio::fs::write(
            &path,
            format!(
                "{}\n{{\"kind\":\"tur",
                tokio::fs::read_to_string(&path).await.unwrap().trim()
            ),
        )
        .await
        .unwrap();
        let lines = store.load("s1").await.unwrap();
        assert_eq!(lines.len(), 1, "corrupt tail must be skipped");
    }

    #[tokio::test]
    async fn session_id_is_sanitized_against_path_traversal() {
        let tmp = TempDir::new().unwrap();
        let store = FileSessionEventStore::new(tmp.path().to_path_buf());
        let path = store.path_for("../../etc/passwd");
        assert!(path.starts_with(tmp.path().join("events")));
        assert!(!path.to_string_lossy().contains(".."));
    }
}
