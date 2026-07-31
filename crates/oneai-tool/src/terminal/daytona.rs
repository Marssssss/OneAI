//! `DaytonaBackend` — a `TerminalBackend` backed by a Daytona serverless
//! workspace over HTTP (Phase 3.3, Stage F).
//!
//! Feature-gated (`daytona`); same supply-chain rationale as
//! [`super::modal`] — no new deps, opt-in.
//!
//! `cleanup(hibernate=true)` → stop (FS preserved, restorable via `start`);
//! `cleanup(hibernate=false)` → destroy the workspace. Daytona's defining
//! trait (vs Modal) is that stopping preserves the filesystem in place, so
//! hibernate is cheap and the same workspace id resumes.

#![cfg(feature = "daytona")]

use std::time::Duration;

use async_trait::async_trait;
use oneai_core::error::{OneAIError, Result};
use serde::{Deserialize, Serialize};

use super::{ExecOptions, ExecResult, SnapshotHandle, TerminalBackend};

#[derive(Serialize)]
struct ExecReq<'a> {
    workspace: &'a str,
    command: &'a str,
    timeout_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    working_dir: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<&'a str>,
}

#[derive(Deserialize)]
struct ExecResp {
    stdout: String,
    #[serde(default)]
    stderr: String,
    exit_code: i64,
}

#[derive(Serialize)]
struct SnapshotReq<'a> {
    workspace: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<&'a str>,
}

#[derive(Deserialize)]
struct SnapshotResp {
    workspace_id: String,
}

#[derive(Serialize)]
struct RestoreReq<'a> {
    workspace: &'a str,
    snapshot_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<&'a str>,
}

#[derive(Serialize)]
struct CleanupReq<'a> {
    workspace: &'a str,
    hibernate: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<&'a str>,
}

/// A `TerminalBackend` backed by a Daytona serverless workspace.
pub struct DaytonaBackend {
    client: reqwest::Client,
    base_url: String,
    workspace: String,
    api_key: Option<String>,
}

impl DaytonaBackend {
    /// Connect to the real Daytona endpoint with a workspace id and optional
    /// API key (env: `DAYTONA_API_KEY`, `DAYTONA_HOST`).
    pub fn new(host: String, api_key: Option<String>) -> Self {
        let base_url = if host.is_empty() {
            "https://app.daytona.io".to_string()
        } else {
            host
        };
        let workspace = "oneai-terminal".to_string();
        Self::with_base_url(base_url, workspace, api_key)
    }

    /// Connect to an explicit base URL (tests point this at a local mock).
    pub fn with_base_url(base_url: String, workspace: String, api_key: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            base_url,
            workspace,
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
            .map_err(|e| OneAIError::Other(format!("daytona HTTP request failed: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(OneAIError::Other(format!(
                "daytona {path} returned {status}: {body}"
            )));
        }
        resp.json::<Resp>()
            .await
            .map_err(|e| OneAIError::Other(format!("daytona {path} decode failed: {e}")))
    }
}

#[async_trait]
impl TerminalBackend for DaytonaBackend {
    fn name(&self) -> &str {
        "daytona"
    }

    fn supports_snapshots(&self) -> bool {
        true
    }

    async fn execute(&self, command: &str, opts: &ExecOptions) -> Result<ExecResult> {
        let working_dir = opts
            .working_dir
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        let req = ExecReq {
            workspace: &self.workspace,
            command,
            timeout_secs: opts.timeout_secs,
            working_dir: working_dir.as_deref(),
            api_key: self.api_key_ref(),
        };
        let resp: ExecResp = self.post_json("/execute", &req).await?;
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
        let req = SnapshotReq {
            workspace: &self.workspace,
            api_key: self.api_key_ref(),
        };
        let resp: SnapshotResp = self.post_json("/snapshot", &req).await?;
        Ok(SnapshotHandle::new(resp.workspace_id, self.name()))
    }

    async fn restore(&self, handle: &SnapshotHandle) -> Result<()> {
        let req = RestoreReq {
            workspace: &self.workspace,
            snapshot_id: &handle.id,
            api_key: self.api_key_ref(),
        };
        self.post_json::<_, serde_json::Value>("/restore", &req)
            .await?;
        Ok(())
    }

    async fn cleanup(&self, hibernate: bool) -> Result<()> {
        let req = CleanupReq {
            workspace: &self.workspace,
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

    async fn mock_server<F, Fut>(handler: F) -> String
    where
        F: Fn() -> Fut + Send + Sync + 'static + Clone,
        Fut: std::future::Future<Output = Json<serde_json::Value>> + Send + 'static,
    {
        let app = Router::new()
            .route("/execute", post(handler.clone()))
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

    async fn exec_handler() -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "stdout": "hello from daytona\n",
            "stderr": "",
            "exit_code": 0,
        }))
    }

    #[tokio::test]
    async fn test_execute_parses_response() {
        let base = mock_server(exec_handler).await;
        let b = DaytonaBackend::with_base_url(base, "ws".into(), None);
        let opts = ExecOptions::new(30, None, 100_000);
        let res = b.execute("echo hi", &opts).await.unwrap();
        assert!(res.success, "{:?}", res.error);
        assert!(res.content.contains("hello from daytona"));
    }

    #[tokio::test]
    async fn test_snapshot_returns_handle() {
        async fn h() -> Json<serde_json::Value> {
            Json(serde_json::json!({ "workspace_id": "ws-42" }))
        }
        let base = mock_server(h).await;
        let b = Arc::new(DaytonaBackend::with_base_url(base, "ws".into(), None));
        let handle = b.snapshot().await.unwrap();
        assert_eq!(handle.id, "ws-42");
        assert_eq!(handle.backend, "daytona");
    }

    #[tokio::test]
    async fn test_restore_cleanup_round_trip() {
        async fn h() -> Json<serde_json::Value> {
            Json(serde_json::json!({}))
        }
        let base = mock_server(h).await;
        let b = Arc::new(DaytonaBackend::with_base_url(base, "ws".into(), None));
        b.restore(&SnapshotHandle::new("ws-1", "daytona"))
            .await
            .unwrap();
        b.cleanup(true).await.unwrap();
        b.cleanup(false).await.unwrap();
    }

    #[test]
    fn test_name_and_supports_snapshots() {
        let b = DaytonaBackend::new(String::new(), None);
        assert_eq!(b.name(), "daytona");
        assert!(b.supports_snapshots());
    }
}
