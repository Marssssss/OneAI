//! Code mode — a sandboxed CPython code-interpreter tool.
//!
//! `CodeInterpreterTool` (`code_interpreter`) lets the model write a Python
//! script that composes multiple OneAI tool calls with imperative control flow
//! (loops / conditionals / retries / aggregation) and in-process variables —
//! the capability discrete structured tool calls cannot express. The script
//! runs in a CPython subprocess isolated by the existing Seatbelt / bwrap
//! [`SandboxBackend`] stack (the same one `ShellTool` uses).
//!
//! Tool calls made *inside* the script are marshalled back to the host over a
//! line-delimited JSON-RPC (stdin/stdout) and fulfilled through
//! [`executor::execute_with_approval`] — the **same** permission-resolver +
//! `InteractionGate::ToolApproval` path a direct model tool call takes. So a
//! high-risk tool invoked from within a script still pauses for human approval;
//! code mode never bypasses the approval model.
//!
//! **Footprint**: registered via `ToolRegistry::register_gated` with a
//! `service_available()` that probes for `python3` on `PATH`. Where `python3`
//! is absent (mobile / native targets without a bundled interpreter) the tool
//! vanishes from the schema entirely — zero footprint, not a broken option the
//! model would try to call. This is the OneAI Footprint Ladder applied to code
//! mode; mobile is "deferred" not "impossible" once a runtime is shipped.
//!
//! See the plan: `~/.claude/plans/hazy-imagining-liskov.md`.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::{InteractionGate, PermissionResolver, Tool};
use oneai_core::{RiskLevel, ToolOutput};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::RwLock;

use crate::executor::{execute_with_approval, ToolExecutorConfig};
use crate::guardian::GuardianContext;
use crate::sandbox::SandboxBackend;

/// The bundled Python bridge — the small process that exec's the user code and
/// shuttles tool calls to the host over stdin/stdout. `include_str!` bakes it
/// into the crate; it is written to a temp file at first use so the sandboxed
/// `python3` can read it (`/tmp` is in the Seatbelt allow-list).
const BRIDGE_SRC: &str = include_str!("oneai_bridge.py");

/// The tool name — also the recursion guard sentinel: a script calling
/// `code_interpreter` from within code mode is rejected at RPC dispatch.
pub const CODE_INTERPRETER_TOOL: &str = "code_interpreter";

/// Cached path to the bridge script (written once into the temp dir).
static BRIDGE_PATH: OnceLock<std::path::PathBuf> = OnceLock::new();

/// Cached `python3` presence probe (`service_available` is a sync hot-path
/// method; we pay the spawn cost exactly once per process).
static PYTHON3_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// A sandboxed CPython code interpreter.
///
/// Holds the shared `tools_map` + gate + resolver + a sandbox backend (not a
/// `ToolExecutor`) so the tool can fulfil script-internal tool calls via
/// [`execute_with_approval`] without forming an `Arc` cycle (the tool lives in
/// the same registry it would query).
pub struct CodeInterpreterTool {
    tools_map: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,
    gate: Arc<dyn InteractionGate>,
    resolver: Option<Arc<dyn PermissionResolver>>,
    config: ToolExecutorConfig,
    sandbox: Arc<dyn SandboxBackend>,
    working_dir: std::path::PathBuf,
    default_timeout_secs: u64,
    max_output_bytes: usize,
    /// The local egress proxy port (#28 Stage 1). When `Some`, scripts route
    /// outbound HTTPS through `http://127.0.0.1:{port}` and the sandbox backend
    /// (constructed by the caller with `NetworkPolicy::LoopbackProxy`) admits
    /// only loopback. `None` → the sandbox is air-gapped (`NetworkPolicy::Denied`).
    proxy_port: Option<u16>,
    /// The Guardian context (#28 Stage 2). When `Some`, tool calls made
    /// *inside* a script are content-reviewed by the same Guardian a direct
    /// call hits — the bridge's per-call dispatch routes through
    /// [`execute_with_approval`] with this context. `None` → the bridge
    /// skips the Guardian (the manual gate / no-UI posture still applies).
    guardian: Option<Arc<GuardianContext>>,
}

impl CodeInterpreterTool {
    /// Construct a code-interpreter tool wired to the same approval pipeline
    /// the `ToolExecutor` uses.
    ///
    /// `tools_map` is the *shared* registry map (`ToolRegistry::tools_map()`),
    /// not the executor itself — this avoids the `Arc` cycle and lets the tool
    /// see tools registered after it.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tools_map: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,
        gate: Arc<dyn InteractionGate>,
        resolver: Option<Arc<dyn PermissionResolver>>,
        config: ToolExecutorConfig,
        sandbox: Arc<dyn SandboxBackend>,
        working_dir: std::path::PathBuf,
    ) -> Self {
        let max_output_bytes = config.max_output_bytes;
        let default_timeout_secs = config.default_timeout_secs;
        Self {
            tools_map,
            gate,
            resolver,
            config,
            sandbox,
            working_dir,
            default_timeout_secs,
            max_output_bytes,
            proxy_port: None,
            guardian: None,
        }
    }

    /// Wire the code-mode egress gate: scripts' outbound HTTPS is routed
    /// through the local proxy on `port` (set `HTTPS_PROXY`/`HTTP_PROXY` env),
    /// and per-host approval flows through the same `InteractionGate` the tool
    /// already holds. The caller must have constructed the `sandbox` backend
    /// with [`crate::sandbox::NetworkPolicy::LoopbackProxy`].
    pub fn with_network_proxy(mut self, port: u16) -> Self {
        self.proxy_port = Some(port);
        self
    }

    /// Wire the Guardian — script-internal tool calls are content-reviewed
    /// by the same [`GuardianContext`] a direct call hits (#28 Stage 2).
    pub fn with_guardian(mut self, guardian: Arc<GuardianContext>) -> Self {
        self.guardian = Some(guardian);
        self
    }

    /// Whether `python3` is reachable on `PATH`. Probed once (sync spawn),
    /// cached for the process lifetime — `service_available` runs on the
    /// tool-definition hot path every iteration.
    fn python3_available(&self) -> bool {
        *PYTHON3_AVAILABLE.get_or_init(|| {
            std::process::Command::new("python3")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        })
    }

    /// Resolve (writing once if needed) the temp path to the bridge script.
    fn bridge_path(&self) -> Result<std::path::PathBuf> {
        if let Some(p) = BRIDGE_PATH.get() {
            return Ok(p.clone());
        }
        let dir = std::env::temp_dir();
        let path = dir.join("oneai_code_bridge.py");
        std::fs::write(&path, BRIDGE_SRC.as_bytes()).map_err(|e| {
            OneAIError::Other(format!("code mode: failed to write bridge script: {e}"))
        })?;
        // Best-effort: another thread may race-write the same path — harmless
        // (same content). `set` returns Ok only for the first setter.
        let _ = BRIDGE_PATH.set(path.clone());
        Ok(BRIDGE_PATH.get().cloned().unwrap_or(path))
    }

    /// Build the JSON tool list injected into the bridge env. Only tools that
    /// are currently visible (`service_available()`) and that are not the code
    /// interpreter itself (recursion guard at the schema level too) are exposed.
    async fn build_tool_list(&self) -> Vec<serde_json::Value> {
        let map = self.tools_map.read().await;
        map.values()
            .filter(|t| {
                let name = t.name();
                name != CODE_INTERPRETER_TOOL && t.service_available()
            })
            .map(|t| {
                serde_json::json!({
                    "name": t.name(),
                    "description": t.description(),
                })
            })
            .collect()
    }

    /// Cap a string to `max_output_bytes` at a UTF-8 boundary (mirrors
    /// `executor::enforce_output_limit`).
    fn cap(s: String, max: usize) -> String {
        if max == 0 || s.len() <= max {
            return s;
        }
        let mut cut = max;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        let mut truncated = String::from(&s[..cut]);
        truncated.push_str(&format!(
            "\n...[output truncated: {} bytes exceeded {} byte limit]",
            s.len(),
            max
        ));
        truncated
    }

    /// Handle one inbound RPC request line from the bridge. Returns the
    /// response object to write back.
    async fn handle_rpc_request(
        &self,
        req: &serde_json::Value,
        traces: &mut Vec<serde_json::Value>,
    ) -> serde_json::Value {
        let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let tool_name = req
            .get("tool")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let args = req
            .get("args")
            .cloned()
            .unwrap_or(serde_json::Value::Object(Default::default()));

        // Recursion guard: a script may not call `code_interpreter` (it would
        // nest subprocesses and re-enter this tool).
        if tool_name == CODE_INTERPRETER_TOOL {
            let err = "cannot call code_interpreter from within code mode";
            traces.push(serde_json::json!({
                "tool": tool_name,
                "args": args,
                "success": false,
                "error": err,
            }));
            return serde_json::json!({
                "id": id,
                "success": false,
                "content": "",
                "error": err,
            });
        }

        let output = execute_with_approval(
            &self.tools_map,
            &self.gate,
            self.resolver.as_ref(),
            self.guardian.as_deref(),
            &self.config,
            &tool_name,
            args.clone(),
        )
        .await
        .unwrap_or_else(|e| ToolOutput {
            success: false,
            content: String::new(),
            error: Some(e.to_string()),
            ..Default::default()
        });

        traces.push(serde_json::json!({
            "tool": tool_name,
            "args": args,
            "success": output.success,
            "content": output.content,
            "error": output.error.clone(),
        }));

        serde_json::json!({
            "id": id,
            "success": output.success,
            "content": output.content,
            "error": output.error,
        })
    }
}

#[async_trait::async_trait]
impl Tool for CodeInterpreterTool {
    fn name(&self) -> &str {
        CODE_INTERPRETER_TOOL
    }

    fn description(&self) -> &str {
        "Execute a Python script in a sandboxed CPython interpreter. The script can call \
        any registered OneAI tool as a keyword-only function (e.g. `read_file(file_path=...)`) \
        — each call is routed back through the normal approval/permission path, exactly like a \
        direct tool call. Use this for multi-step operations with complex control flow \
        (loops/conditionals/retries/aggregation) or in-process data transformation that would \
        otherwise require many round-trips. The script's stdout is returned as the result. \
        Network is disabled in the sandbox; use the `shell` tool for networked operations."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Python source to execute. Print results to stdout. \
                    Call OneAI tools as keyword-only functions: e.g. `read_file(file_path=\"x\")`."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Optional max execution time in seconds (default 60).",
                    "default": 60
                }
            },
            "required": ["code"]
        })
    }

    fn risk_level(&self) -> RiskLevel {
        // Arbitrary code execution — must clear the approval bar.
        RiskLevel::High
    }

    fn service_available(&self) -> bool {
        self.python3_available()
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let code = args
            .get("code")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                OneAIError::Other("code_interpreter: 'code' (string) is required".into())
            })?
            .to_string();
        let timeout_secs = args
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(self.default_timeout_secs)
            .clamp(1, 600);

        let bridge_path = self.bridge_path()?;
        let tool_list = self.build_tool_list().await;
        let tools_json = serde_json::to_string(&tool_list).unwrap_or_else(|_| "[]".into());

        // The inner command the sandbox wraps: isolated python (-I) + no .pyc (-B).
        let inner_cmd = format!(
            "python3 -I -B {}",
            shell_quote(&bridge_path.to_string_lossy())
        );

        let wrapped = self.sandbox.wrap_command(&inner_cmd, &self.working_dir)?;

        let (shell, shell_arg) = crate::tool_interfaces::resolve_shell();

        let mut command = Command::new(shell);
        command
            .arg(shell_arg)
            .arg(&wrapped.shell_command)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .envs(&wrapped.env_vars)
            .env("ONEAI_CODE", &code)
            .env("ONEAI_TOOLS", &tools_json);

        // #28 Stage 1 — route the script's outbound HTTPS through the local
        // egress proxy. The sandbox backend (constructed with
        // `NetworkPolicy::LoopbackProxy`) admits only loopback, so direct
        // internet egress is blocked; the proxy enforces the per-host gate.
        if let Some(port) = self.proxy_port {
            let proxy_url = format!("http://127.0.0.1:{port}");
            command
                .env("HTTPS_PROXY", &proxy_url)
                .env("HTTP_PROXY", &proxy_url)
                // Don't re-proxy loopback / the proxy itself.
                .env("NO_PROXY", "127.0.0.1,localhost");
        }

        // kill_on_drop: if the execute future is cancelled (e.g. a test
        // runtime drops the task, or the agent loop is interrupted mid-script),
        // the Child is dropped → tokio SIGKILLs the direct child. Without this,
        // a dropped future would orphan the python bridge. Belt-and-suspenders
        // alongside the explicit `start_kill` on the timeout/error paths below.
        command.kill_on_drop(true);

        let mut child = command
            .spawn()
            .map_err(|e| OneAIError::Other(format!("code mode: failed to spawn python3: {e}")))?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        // Drain stderr concurrently so a chatty python can't deadlock the pipe
        // while we block on the RPC channel. Captured for diagnostics on the
        // error path; the bridge sends the user's own stderr in `done`.
        let mut stderr_task = tokio::spawn(drain_stderr(stderr));

        let result = tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            run_rpc_loop(self, stdin, stdout),
        )
        .await;

        match result {
            Ok(Ok(out)) => {
                let _ = reap_child(&mut child).await;
                let _ = drain_stderr_bounded(&mut stderr_task).await;
                Ok(out)
            }
            Ok(Err(e)) => {
                let _ = child.start_kill();
                let _ = reap_child(&mut child).await;
                let _ = drain_stderr_bounded(&mut stderr_task).await;
                Ok(out_from_error(&e))
            }
            // Timed out — kill the runaway script, surface a clear timeout.
            Err(_) => {
                let _ = child.start_kill();
                let _ = reap_child(&mut child).await;
                let drained = drain_stderr_bounded(&mut stderr_task).await;
                let stderr_s = String::from_utf8_lossy(&drained);
                Ok(ToolOutput {
                    success: false,
                    content: String::new(),
                    error: Some(format!(
                        "code mode: script timed out after {timeout_secs}s\n--- python stderr ---\n{stderr_s}"
                    )),
                    ..Default::default()
                })
            }
        }
    }
}

/// Reap the child process with a hard bound. `start_kill`/exit should make
/// `wait()` return instantly, but bound it anyway — a wedged child must never
/// turn a script timeout into an indefinite hang (the #28 Stage-3 CI failure
/// mode: the test step blocked forever after the 2s timeout fired).
async fn reap_child(child: &mut tokio::process::Child) {
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
}

/// Drain the stderr task with a hard bound. `drain_stderr` does a `read_to_end`
/// that blocks until EOF — fine when the child exited cleanly (pipe closes
/// immediately), but if a descendant the child forked outlives the killed
/// direct child, the stderr write-end stays open and the drain blocks forever.
/// Bound it: on expiry, abort the task (abandon partial stderr) so the caller
/// always proceeds within ~3s rather than hanging.
async fn drain_stderr_bounded(task: &mut tokio::task::JoinHandle<Vec<u8>>) -> Vec<u8> {
    match tokio::time::timeout(Duration::from_secs(3), &mut *task).await {
        Ok(Ok(buf)) => buf,
        Ok(Err(_)) => Vec::new(),
        Err(_) => {
            task.abort();
            Vec::new()
        }
    }
}

/// Drive the stdin/stdout JSON-RPC loop until the bridge sends `done` or EOF.
async fn run_rpc_loop(
    tool: &CodeInterpreterTool,
    mut stdin: ChildStdin,
    stdout: ChildStdout,
) -> std::result::Result<ToolOutput, String> {
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();
    let mut traces: Vec<serde_json::Value> = Vec::new();

    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => {
                return Err("code mode: bridge exited without sending `done`".to_string());
            }
            Err(e) => return Err(format!("code mode: error reading bridge stdout: {e}")),
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => return Err(format!("code mode: bad JSON from bridge: {e} :: {line}")),
        };

        let typ = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if typ == "done" {
            let stdout_buf = msg
                .get("stdout")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let stderr_buf = msg
                .get("stderr")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let script_error = msg
                .get("error")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let success = script_error.is_none();
            let content = serde_json::to_string(&serde_json::json!({
                "stdout": CodeInterpreterTool::cap(stdout_buf, tool.max_output_bytes),
                "stderr": CodeInterpreterTool::cap(stderr_buf, tool.max_output_bytes),
                "traces": traces,
            }))
            .unwrap_or_else(|_| "{}".into());

            return Ok(ToolOutput {
                success,
                content,
                error: script_error,
                ..Default::default()
            });
        }

        // Otherwise it's a `call` request — handle + write the response back.
        let response = tool.handle_rpc_request(&msg, &mut traces).await;
        let mut response_line = serde_json::to_string(&response)
            .map_err(|e| format!("code mode: failed to serialize RPC response: {e}"))?;
        response_line.push('\n');
        stdin
            .write_all(response_line.as_bytes())
            .await
            .map_err(|e| format!("code mode: failed to write RPC response: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("code mode: failed to flush RPC response: {e}"))?;
    }
}

/// Drain a child's stderr fully into a byte buffer (best-effort).
async fn drain_stderr(mut stderr: ChildStderr) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = stderr.read_to_end(&mut buf).await;
    buf
}

fn out_from_error(e: &str) -> ToolOutput {
    ToolOutput {
        success: false,
        content: String::new(),
        error: Some(e.to_string()),
        ..Default::default()
    }
}

/// Minimal POSIX shell quoting for a single argument (the bridge path). The
/// path is a temp dir, but quote defensively in case `TMPDIR` contains spaces.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::ToolExecutorConfig;
    use crate::interaction_gate::NoopInteractionGate;
    use crate::local_tools::CalculatorTool;
    use crate::registry::ToolRegistry;
    use crate::sandbox::{NetworkPolicy, RegexBackend, SandboxBackend};

    /// Build a tool wired to a fresh registry holding `calculator`, with a
    /// no-op gate and the regex (pass-through) sandbox backend.
    ///
    /// The sandbox is deliberately a `RegexBackend` (no real process isolation),
    /// NOT `default_sandbox_backend_with_policy` — that selector picks the
    /// platform backend (bwrap/docker), and the CI runner has neither bwrap nor
    /// the `oneai-sandbox:latest` Docker image, so `docker run oneai-sandbox`
    /// would fail to find the image and the bridge would "exit without sending
    /// `done`". The sandbox is orthogonal to what these tests exercise (the RPC
    /// loop); a benign pass-through backend is the correct setup. Production
    /// wiring (the AppBuilder) selects the real platform backend separately.
    async fn make_tool() -> (CodeInterpreterTool, Arc<ToolRegistry>) {
        let registry = Arc::new(ToolRegistry::new());
        registry
            .register(Arc::new(CalculatorTool::new()))
            .await
            .unwrap();
        let working_dir = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let sandbox = Arc::new(
            RegexBackend::coding_defaults(&working_dir).with_network_policy(NetworkPolicy::Denied),
        ) as Arc<dyn SandboxBackend>;
        let tool = CodeInterpreterTool::new(
            registry.tools_map(),
            Arc::new(NoopInteractionGate),
            None,
            ToolExecutorConfig::default(),
            sandbox,
            working_dir,
        );
        (tool, registry)
    }

    fn python3_present() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn calls_tool_over_rpc_and_returns_stdout() {
        if !python3_present() {
            return;
        }
        let (tool, _registry) = make_tool().await;
        // calculator({expression:"2+3"}) -> "5", printed.
        let code = "print(calculator(expression='2+3'))";
        let out = tool
            .execute(serde_json::json!({"code": code, "timeout": 30}))
            .await
            .unwrap();
        assert!(out.success, "expected success, error: {:?}", out.error);
        let parsed: serde_json::Value = serde_json::from_str(&out.content).unwrap();
        assert!(parsed["stdout"].as_str().unwrap().trim() == "5");
        let trace = parsed["traces"].as_array().unwrap();
        assert_eq!(trace[0]["tool"], "calculator");
        assert!(trace[0]["success"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn script_error_surfaced_as_tool_error() {
        if !python3_present() {
            return;
        }
        let (tool, _registry) = make_tool().await;
        let code = "raise RuntimeError('boom')";
        let out = tool
            .execute(serde_json::json!({"code": code, "timeout": 30}))
            .await
            .unwrap();
        assert!(!out.success);
        let err = out.error.unwrap();
        assert!(err.contains("boom"));
    }

    #[tokio::test]
    async fn recursion_guard_rejects_code_interpreter_call() {
        if !python3_present() {
            return;
        }
        let (tool, _registry) = make_tool().await;
        // The bridge won't expose code_interpreter as a fn (build_tool_list
        // excludes it), but if a script invokes it via _call_tool by name the
        // host must reject. We synthesize an RPC request directly.
        let req = serde_json::json!({
            "id": 1, "type": "call", "tool": "code_interpreter", "args": {"code": "x"}
        });
        let mut traces = Vec::new();
        let resp = tool.handle_rpc_request(&req, &mut traces).await;
        assert!(!resp["success"].as_bool().unwrap());
        assert!(resp["error"]
            .as_str()
            .unwrap()
            .contains("cannot call code_interpreter"));
        assert_eq!(traces.len(), 1);
    }

    #[tokio::test]
    async fn timeout_kills_infinite_loop() {
        if !python3_present() {
            return;
        }
        let (tool, _registry) = make_tool().await;
        let code = "while True:\n    pass";
        let out = tool
            .execute(serde_json::json!({"code": code, "timeout": 2}))
            .await
            .unwrap();
        assert!(!out.success);
        assert!(out.error.as_ref().unwrap().contains("timed out"));
    }

    #[test]
    fn cap_truncates_at_utf8_boundary() {
        let s = "x".repeat(100);
        let capped = CodeInterpreterTool::cap(s, 10);
        assert!(capped.len() < 100);
        assert!(capped.contains("truncated"));
        assert!(std::str::from_utf8(capped.as_bytes()).is_ok());
    }

    #[test]
    fn cap_preserves_small_output() {
        let s = "hello".to_string();
        let capped = CodeInterpreterTool::cap(s.clone(), 1000);
        assert_eq!(capped, s);
    }
}
