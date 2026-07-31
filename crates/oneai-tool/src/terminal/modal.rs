//! `ModalBackend` — a `TerminalBackend` backed by a Modal serverless
//! sandbox over HTTP (Phase 3.3, Stage F).
//!
//! Feature-gated (`modal`): `reqwest` / `serde_json` are already hard deps
//! of `oneai-tool`, so this feature adds **zero** new dependencies — it only
//! gates whether the module is compiled (off by default per 戒律 #3).
//!
//! The endpoint shape (`POST /run`, `/snapshot`, `/restore`, `/cleanup`) is
//! the trait-surface contract; a real Modal integration maps these to Modal's
//! sandbox API. The backend carries an optional API key; without one, requests
//! are anonymous (and the serverless provider rejects them). Tests point the
//! backend at a local axum mock server via [`ModalBackend::with_base_url`].

#![cfg(feature = "modal")]

use std::time::Duration;

use async_trait::async_trait;
use oneai_core::error::{OneAIError, Result};
use serde::{Deserialize, Serialize};

use super::{ExecOptions, ExecResult, SnapshotHandle, TerminalBackend};

#[derive(Serialize)]
struct RunRequest<'a> {
    app: &'a str,
    command: &'a str,
    timeout_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    working_dir: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<&'a str>,
}

#[derive(Deserialize)]
struct RunResponse {
    stdout: String,
    #[serde(default)]
    stderr: String,
    exit_code: i64,
}

#[derive(Serialize)]
struct SnapshotRequest<'a> {
    app: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<&'a str>,
}

#[derive(Deserialize)]
struct SnapshotResponse {
    snapshot_id: String,
}

#[derive(Serialize)]
struct RestoreRequest<'a> {
    app: &'a str,
    snapshot_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<&'a str>,
}

#[derive(Serialize)]
struct CleanupRequest<'a> {
    app: &'a str,
    hibernate: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<&'a str>,
}

/// A `TerminalBackend` backed by a Modal serverless sandbox.
///
/// `cleanup(hibernate=true)` → snapshot+terminate (Modal snapshots the FS
/// and tears down the running sandbox, restorable via `restore`).
/// `cleanup(hibernate=false)` → terminate without snapshotting (destroy).
pub struct ModalBackend {
    client: reqwest::Client,
    base_url: String,
    app: String,
    api_key: Option<String>,
}

impl ModalBackend {
    /// Connect to the real Modal endpoint (`https://modal.com`) with the
    /// given app name and optional API key (env: `MODAL_TOKEN`).
    pub fn new(app: String, api_key: Option<String>) -> Self {
        Self::with_base_url("https://modal.com".to_string(), app, api_key)
    }

    /// Connect to an explicit base URL (tests point this at a local axum mock).
    pub fn with_base_url(base_url: String, app: String, api_key: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            base_url,
            app,
            api_key,
        }
    }

    fn api_key_ref(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    async fn post_json<Req: Serialize, Resp: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        req: &Req,
    ) -> Result<Resp> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .client
            .post(&url)
            .json(req)
            .send()
            .await
            .map_err(|e| OneAIError::Other(format!("modal HTTP request failed: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(OneAIError::Other(format!(
                "modal {path} returned {status}: {body}"
            )));
        }
        resp.json::<Resp>()
            .await
            .map_err(|e| OneAIError::Other(format!("modal {path} decode failed: {e}")))
    }
}

#[async_trait]
impl TerminalBackend for ModalBackend {
    fn name(&self) -> &str {
        "modal"
    }

    fn supports_snapshots(&self) -> bool {
        true
    }

    async fn execute(&self, command: &str, opts: &ExecOptions) -> Result<ExecResult> {
        let working_dir = opts
            .working_dir
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        let req = RunRequest {
            app: &self.app,
            command,
            timeout_secs: opts.timeout_secs,
            working_dir: working_dir.as_deref(),
            api_key: self.api_key_ref(),
        };
        let resp: RunResponse = self.post_json("/run", &req).await?;
        let success = resp.exit_code == 0;
        let content = super::format_and_truncate(&resp.stdout, &resp.stderr, opts.max_output_bytes);
        Ok(ExecResult {
            success,
            content,
            error: if success {
                None
            } else {
                Some(format!("Exit code: {}", resp.exit_code))
            },
        })
    }

    async fn snapshot(&self) -> Result<SnapshotHandle> {
        let req = SnapshotRequest {
            app: &self.app,
            api_key: self.api_key_ref(),
        };
        let resp: SnapshotResponse = self.post_json("/snapshot", &req).await?;
        Ok(SnapshotHandle::new(resp.snapshot_id, self.name()))
    }

    async fn restore(&self, handle: &SnapshotHandle) -> Result<()> {
        let req = RestoreRequest {
            app: &self.app,
            snapshot_id: &handle.id,
            api_key: self.api_key_ref(),
        };
        self.post_json::<_, serde_json::Value>("/restore", &req)
            .await?;
        Ok(())
    }

    async fn cleanup(&self, hibernate: bool) -> Result<()> {
        let req = CleanupRequest {
            app: &self.app,
            hibernate,
            api_key: self.api_key_ref(),
        };
        self.post_json::<_, serde_json::Value>("/cleanup", &req)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Json, Router};
    use std::sync::Arc;

    /// Spin up a one-route-per-path axum mock returning canned JSON. Every
    /// path shares the same handler fn (each test builds its own server).
    async fn mock_server<F, Fut>(handler: F) -> String
    where
        F: Fn() -> Fut + Send + Sync + 'static + Clone,
        Fut: std::future::Future<Output = Json<serde_json::Value>> + Send + 'static,
    {
        let app = Router::new()
            .route("/run", post(handler.clone()))
            .route("/snapshot", post(handler.clone()))
            .route("/restore", post(handler.clone()))
            .route("/cleanup", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    async fn run_handler() -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "stdout": "hello from modal\n",
            "stderr": "",
            "exit_code": 0,
        }))
    }

    #[tokio::test]
    async fn test_execute_parses_run_response() {
        let base = mock_server(run_handler).await;
        let b = ModalBackend::with_base_url(base, "oneai-terminal".into(), None);
        let opts = ExecOptions::new(30, None, 100_000);
        let res = b.execute("echo hi", &opts).await.unwrap();
        assert!(res.success, "{:?}", res.error);
        assert!(res.content.contains("hello from modal"));
    }

    #[tokio::test]
    async fn test_snapshot_returns_handle() {
        async fn h() -> Json<serde_json::Value> {
            Json(serde_json::json!({ "snapshot_id": "snap-123" }))
        }
        let base = mock_server(h).await;
        let b = Arc::new(ModalBackend::with_base_url(base, "app".into(), None));
        let handle = b.snapshot().await.unwrap();
        assert_eq!(handle.id, "snap-123");
        assert_eq!(handle.backend, "modal");
    }

    #[tokio::test]
    async fn test_cleanup_restore_round_trip() {
        async fn h() -> Json<serde_json::Value> {
            Json(serde_json::json!({}))
        }
        let base = mock_server(h).await;
        let b = Arc::new(ModalBackend::with_base_url(base, "app".into(), None));
        b.restore(&SnapshotHandle::new("snap-1", "modal"))
            .await
            .unwrap();
        b.cleanup(true).await.unwrap();
        b.cleanup(false).await.unwrap();
    }

    #[test]
    fn test_name_and_supports_snapshots() {
        let b = ModalBackend::new("app".into(), None);
        assert_eq!(b.name(), "modal");
        assert!(b.supports_snapshots());
    }
}
