//! IPC server — the supervisor daemon's accept loop.
//!
//! Listens on an [`IpcListener`], and for each connection reads
//! newline-delimited JSON [`Request`]s, dispatches them to the
//! [`Supervisor`], and writes [`Response`] (or streaming [`StreamLine`]s for
//! `rpc_stream`). The `rpc_stream` path bridges the agent's
//! [`AgentLoopObserver`] callbacks to the connection via a dedicated writer
//! arm draining an unbounded channel — events flow **live** while the turn
//! runs concurrently (mirrors `oneai-studio::ws`).

use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use oneai_trace::TraceContext;

use crate::error::Result;
use crate::protocol::{decode, encode, Request, Response, RpcMethod, StreamLine};
use crate::registry::{InstanceRegistry, InstanceSpec};
use crate::supervisor::{EventSink, Supervisor};
use crate::transport::IpcListener;

/// Daemon configuration.
#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    /// IPC socket path (Unix) / named-pipe name (Windows).
    pub socket_path: PathBuf,
    /// Root dir for `instances.json`.
    pub root_dir: PathBuf,
}

impl SupervisorConfig {
    /// Defaults: `~/.oneai/server.sock` + `~/.oneai/server/`.
    pub fn default_config() -> Self {
        Self {
            socket_path: crate::transport::default_socket_path(),
            root_dir: crate::transport::default_server_dir(),
        }
    }
}

/// A bound supervisor server.
pub struct SupervisorServer {
    supervisor: Arc<Supervisor>,
    listener: IpcListener,
}

impl SupervisorServer {
    /// Bind a listener; does not yet accept.
    pub async fn bind(config: SupervisorConfig, supervisor: Arc<Supervisor>) -> Result<Self> {
        let listener = IpcListener::bind(&config.socket_path).await?;
        Ok(Self {
            supervisor,
            listener,
        })
    }

    /// Run the accept loop until the listener errors or is shut down.
    pub async fn serve(mut self) -> Result<()> {
        loop {
            let stream = self.listener.accept().await?;
            let sup = self.supervisor.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, sup).await {
                    tracing::warn!(error = %e, "supervisor: connection ended");
                }
            });
        }
    }
}

/// Convenience: build a registry + supervisor over `runner` and serve.
pub async fn serve(
    config: SupervisorConfig,
    runner: Arc<dyn crate::SupervisorRunner>,
) -> Result<()> {
    serve_with_supervisor(config, runner, None).await
}

/// As [`serve`] but attach an OTEL [`TraceContext`].
pub async fn serve_with_trace(
    config: SupervisorConfig,
    runner: Arc<dyn crate::SupervisorRunner>,
    trace: TraceContext,
) -> Result<()> {
    serve_with_supervisor(config, runner, Some(trace)).await
}

async fn serve_with_supervisor(
    config: SupervisorConfig,
    runner: Arc<dyn crate::SupervisorRunner>,
    trace: Option<TraceContext>,
) -> Result<()> {
    let registry = Arc::new(InstanceRegistry::new(config.root_dir.clone()).await?);
    // Reconcile any leftover Running instances from a prior crashed run.
    registry.recover_after_restart().await?;
    let supervisor = Arc::new(Supervisor::new(runner, registry, trace));
    let server = SupervisorServer::bind(config, supervisor).await?;
    server.serve().await
}

// ─── Per-connection handler ───────────────────────────────────────────────────

async fn handle_connection(
    stream: crate::transport::IpcStream,
    supervisor: Arc<Supervisor>,
) -> Result<()> {
    let (read, mut write) = tokio::io::split(stream);
    let mut reader = BufReader::new(read);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(()); // client closed
        }
        let trimmed = line.trim_end_matches('\n');
        if trimmed.is_empty() {
            continue;
        }
        let value = match decode(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let resp = Response::err(0, format!("bad request: {e}"));
                write.write_all(encode(&resp)?.as_bytes()).await?;
                continue;
            }
        };
        let request: Request = match serde_json::from_value(value) {
            Ok(r) => r,
            Err(e) => {
                let resp = Response::err(0, format!("bad request: {e}"));
                write.write_all(encode(&resp)?.as_bytes()).await?;
                continue;
            }
        };
        match dispatch(&request, &supervisor).await {
            DispatchOutcome::Single(response) => {
                write.write_all(encode(&response)?.as_bytes()).await?;
            }
            DispatchOutcome::Stream(mut rx) => {
                while let Some(line_str) = rx.recv().await {
                    if write.write_all(line_str.as_bytes()).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }
}

enum DispatchOutcome {
    Single(Response),
    Stream(mpsc::UnboundedReceiver<String>),
}

async fn dispatch(request: &Request, supervisor: &Arc<Supervisor>) -> DispatchOutcome {
    let id = request.id;
    match request.method {
        RpcMethod::Spawn => match parse_spawn_params(&request.params) {
            Ok(spec) => match supervisor.spawn(spec).await {
                Ok(inst_id) => {
                    DispatchOutcome::Single(Response::ok(id, serde_json::json!({ "id": inst_id })))
                }
                Err(e) => DispatchOutcome::Single(Response::err(id, e.to_string())),
            },
            Err(e) => DispatchOutcome::Single(Response::err(id, e)),
        },
        RpcMethod::List => {
            let list = supervisor.list().await;
            DispatchOutcome::Single(Response::ok(
                id,
                serde_json::to_value(&list).unwrap_or(serde_json::Value::Null),
            ))
        }
        RpcMethod::Status => {
            let inst_id = match request.params.get("id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return DispatchOutcome::Single(Response::err(id, "missing id")),
            };
            match supervisor.status(&inst_id).await {
                Ok(info) => DispatchOutcome::Single(Response::ok(
                    id,
                    serde_json::to_value(&info).unwrap_or(serde_json::Value::Null),
                )),
                Err(e) => DispatchOutcome::Single(Response::err(id, e.to_string())),
            }
        }
        RpcMethod::Stop => {
            let inst_id = match request.params.get("id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return DispatchOutcome::Single(Response::err(id, "missing id")),
            };
            match supervisor.stop(&inst_id).await {
                Ok(()) => DispatchOutcome::Single(Response::ok(
                    id,
                    serde_json::json!({ "stopped": true }),
                )),
                Err(e) => DispatchOutcome::Single(Response::err(id, e.to_string())),
            }
        }
        RpcMethod::Rpc => match parse_rpc_params(&request.params) {
            Ok((inst_id, message)) => match supervisor.rpc(&inst_id, &message).await {
                Ok(summary) => DispatchOutcome::Single(Response::ok(
                    id,
                    serde_json::to_value(&summary).unwrap_or(serde_json::Value::Null),
                )),
                Err(e) => DispatchOutcome::Single(Response::err(id, e.to_string())),
            },
            Err(e) => DispatchOutcome::Single(Response::err(id, e)),
        },
        RpcMethod::RpcStream => match parse_rpc_params(&request.params) {
            Ok((inst_id, message)) => {
                let (tx, rx) = mpsc::unbounded_channel::<String>();
                let sink = Arc::new(LineSink { id, tx: tx.clone() }) as Arc<dyn EventSink>;
                let sup = supervisor.clone();
                tokio::spawn(async move {
                    let result = sup.rpc_stream(&inst_id, &message, sink).await;
                    let terminal = match result {
                        Ok(summary) => StreamLine::done_ok(
                            id,
                            serde_json::to_value(&summary).unwrap_or(serde_json::Value::Null),
                        ),
                        Err(e) => StreamLine::done_err(id, e.to_string()),
                    };
                    if let Ok(s) = encode(&terminal) {
                        let _ = tx.send(s);
                    }
                    // `tx` drops → `rx` closes after the terminal line is read.
                });
                DispatchOutcome::Stream(rx)
            }
            Err(e) => DispatchOutcome::Single(Response::err(id, e)),
        },
    }
}

/// A sink that wraps each serialized `EngineYield` value as a
/// `StreamLine::event` line and pushes the encoded string into an unbounded
/// channel (drained by the connection's writer arm).
struct LineSink {
    id: u64,
    tx: mpsc::UnboundedSender<String>,
}

impl EventSink for LineSink {
    fn emit(&self, yield_json: serde_json::Value) {
        let line = StreamLine::event(self.id, yield_json);
        if let Ok(s) = encode(&line) {
            let _ = self.tx.send(s);
        }
    }
}

fn parse_spawn_params(params: &serde_json::Value) -> std::result::Result<InstanceSpec, String> {
    let id = params
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or("missing id")?
        .to_string();
    let domain = params
        .get("domain")
        .and_then(|v| v.as_str())
        .unwrap_or("coding")
        .to_string();
    let model = params
        .get("model")
        .and_then(|v| v.as_str())
        .map(String::from);
    let user = params
        .get("user")
        .and_then(|v| v.as_str())
        .map(String::from);
    Ok(InstanceSpec {
        id,
        domain,
        model,
        user,
        created_at: chrono::Utc::now(),
    })
}

fn parse_rpc_params(params: &serde_json::Value) -> std::result::Result<(String, String), String> {
    let id = params
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or("missing id")?
        .to_string();
    let message = params
        .get("message")
        .and_then(|v| v.as_str())
        .ok_or("missing message")?
        .to_string();
    Ok((id, message))
}
