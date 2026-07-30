//! IPC client — used by native apps (and the CLI) to drive the supervisor.
//!
//! Reconnect after a kill via [`SupervisorClient::connect_with_recover`]; the
//! durable registry survives the restart, so a reconnected client can list
//! prior instances (marked `Crashed("supervisor_restart")`) and re-spawn.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;

use crate::error::{Result, SupervisorError};
use crate::protocol::{encode, Request, Response, RpcMethod, StreamLine};
use crate::registry::{InstanceInfo, InstanceSpec};
use crate::runner::TurnSummary;
use crate::supervisor::Event;
use crate::transport::{self, IpcStream};

/// A client connected to a supervisor daemon.
pub struct SupervisorClient {
    inner: Arc<Mutex<ClientInner>>,
}

struct ClientInner {
    read: BufReader<tokio::io::ReadHalf<IpcStream>>,
    write: tokio::io::WriteHalf<IpcStream>,
    next_id: u64,
}

impl SupervisorClient {
    /// Connect to a daemon at `path` (socket / named pipe).
    pub async fn connect(path: &Path) -> Result<Self> {
        let stream = transport::connect(path).await?;
        let (read, write) = tokio::io::split(stream);
        Ok(Self {
            inner: Arc::new(Mutex::new(ClientInner {
                read: BufReader::new(read),
                write,
                next_id: 1,
            })),
        })
    }

    /// Connect with exponential backoff retries (reconnect after a kill).
    pub async fn connect_with_recover(path: &Path, retries: usize) -> Result<Self> {
        let mut delay = Duration::from_millis(50);
        let mut last_err: Option<SupervisorError> = None;
        for _ in 0..=retries {
            match Self::connect(path).await {
                Ok(c) => return Ok(c),
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(2));
                }
            }
        }
        Err(last_err.unwrap_or_else(|| SupervisorError::Protocol("connect failed".to_string())))
    }

    async fn call(
        &self,
        method: RpcMethod,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let mut inner = self.inner.lock().await;
        let id = inner.next_id;
        inner.next_id += 1;
        let request = Request { id, method, params };
        let line = encode(&request)?;
        inner.write.write_all(line.as_bytes()).await?;
        inner.write.flush().await?;

        let mut buf = String::new();
        let n = inner.read.read_line(&mut buf).await?;
        if n == 0 {
            return Err(SupervisorError::Protocol("connection closed".to_string()));
        }
        let resp: Response = serde_json::from_str(buf.trim_end_matches('\n'))?;
        drop(inner);

        if resp.ok {
            resp.result
                .ok_or_else(|| SupervisorError::Protocol("ok response missing result".to_string()))
        } else {
            Err(SupervisorError::Instance(
                resp.error.unwrap_or_else(|| "unknown error".to_string()),
            ))
        }
    }

    /// Spawn a new supervised instance. Returns the instance id.
    pub async fn spawn(&self, spec: &InstanceSpec) -> Result<String> {
        let params = serde_json::to_value(spec)?;
        let result = self.call(RpcMethod::Spawn, params).await?;
        Ok(result
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| SupervisorError::Protocol("spawn response missing id".to_string()))?
            .to_string())
    }

    /// List all instances.
    pub async fn list(&self) -> Result<Vec<InstanceInfo>> {
        let result = self.call(RpcMethod::List, serde_json::Value::Null).await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Stop and unregister an instance.
    pub async fn stop(&self, id: &str) -> Result<()> {
        let _ = self
            .call(RpcMethod::Stop, serde_json::json!({ "id": id }))
            .await?;
        Ok(())
    }

    /// Query one instance's status.
    pub async fn status(&self, id: &str) -> Result<InstanceInfo> {
        let result = self
            .call(RpcMethod::Status, serde_json::json!({ "id": id }))
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Run one turn, returning the final summary.
    pub async fn rpc(&self, id: &str, task: &str) -> Result<TurnSummary> {
        let result = self
            .call(
                RpcMethod::Rpc,
                serde_json::json!({ "id": id, "message": task }),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Run one turn, returning a live stream of [`Event`]s. The stream ends
    /// (returns `None`) after the terminal `done` line.
    ///
    /// Holds the connection's lock for the duration of the stream, so no other
    /// call can be issued mid-stream (single-flight on one connection).
    pub fn rpc_stream(&self, id: &str, task: &str) -> impl Stream<Item = Result<Event>> + '_ {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event>>(64);
        let inner = self.inner.clone();
        let id = id.to_string();
        let task = task.to_string();
        tokio::spawn(async move {
            let mut guard = inner.lock().await;
            let req_id = guard.next_id;
            guard.next_id += 1;
            let request = Request {
                id: req_id,
                method: RpcMethod::RpcStream,
                params: serde_json::json!({ "id": id, "message": task }),
            };
            let line = match encode(&request) {
                Ok(l) => l,
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            };
            if guard.write.write_all(line.as_bytes()).await.is_err() {
                return;
            }
            let _ = guard.write.flush().await;
            let mut buf = String::new();
            loop {
                buf.clear();
                let n = match guard.read.read_line(&mut buf).await {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = tx.send(Err(SupervisorError::from(e))).await;
                        return;
                    }
                };
                if n == 0 {
                    break;
                }
                let parsed: std::result::Result<StreamLine, _> =
                    serde_json::from_str(buf.trim_end_matches('\n'));
                let line = match parsed {
                    Ok(l) => l,
                    Err(e) => {
                        let _ = tx.send(Err(SupervisorError::from(e))).await;
                        return;
                    }
                };
                if let Some(event_value) = line.event {
                    if let Ok(event) = serde_json::from_value::<Event>(event_value) {
                        let _ = tx.send(Ok(event)).await;
                    }
                }
                if line.done.unwrap_or(false) {
                    if let Some(err) = line.error {
                        let _ = tx.send(Err(SupervisorError::Instance(err))).await;
                    }
                    break;
                }
            }
        });
        ReceiverStream::new(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SupervisorClient>();
    }
}
