//! TerminalBackend — abstract the shell-execution surface so `ShellTool` no
//! longer calls `tokio::process::Command::new` directly, but holds an
//! `Arc<dyn TerminalBackend>` (Phase 3.3).
//!
//! This mirrors the `CronScheduler` seam (`oneai-core/src/traits.rs`): a small
//! required surface (`name`/`execute`) + capability flag (`supports_snapshots`)
//! + safe-default no-op lifecycle methods (`snapshot`/`restore`/`cleanup`).
//!
//! ## Safety boundary (load-bearing)
//!
//! Command-string-level pre-flight safety (blocked patterns, shell file-write
//! detection, empty-command check) stays in `ShellTool::execute` and runs
//! **before** `backend.execute()` — so it applies uniformly to *every* backend
//! (a dangerous command must never reach a Modal container). Execution
//! mechanics (shell resolution, spawn, timeout, output formatting + UTF-8
//! truncation) live inside each backend.
//!
//! ## Backends
//!
//! - [`LocalBackend`] — current behavior, verbatim. Composes an optional
//!   [`crate::sandbox::SandboxBackend`] for command wrapping. Zero behavior
//!   change from the pre-refactor `ShellTool`.
//! - `DockerTerminalBackend` (Stage C) — real long-lived container lifecycle.
//! - `ModalBackend` / `DaytonaBackend` (Stage F, feature-gated) — serverless
//!   HTTP terminals.
//!
//! The trait lives in `oneai-tool` (not `oneai-core`) because `LocalBackend`
//! needs `tokio::process::Command`, which is a `oneai-tool` dependency —
//! `oneai-core` stays pure.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use oneai_core::error::{OneAIError, Result};

use crate::sandbox::SandboxBackend;

#[cfg(feature = "daytona")]
pub mod daytona;
pub mod docker;
pub mod file_sync;
#[cfg(feature = "modal")]
pub mod modal;

// ─── Supporting structs ──────────────────────────────────────────────────────

/// Options passed to [`TerminalBackend::execute`]. Built by `ShellTool` from
/// its configured fields + the per-call `timeout` argument.
///
/// `#[non_exhaustive]` per the v0.2.0 stability commitment — downstream must
/// use [`ExecOptions::new`].
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ExecOptions {
    /// Timeout in seconds (already clamped to the tool's max by `ShellTool`).
    pub timeout_secs: u64,
    /// Working directory for the command. `None` = inherit / backend default
    /// (for `LocalBackend`, the current dir).
    pub working_dir: Option<PathBuf>,
    /// Maximum output size in bytes; output is truncated to a UTF-8 boundary
    /// if exceeded (prevents context overflow).
    pub max_output_bytes: usize,
}

impl ExecOptions {
    pub fn new(timeout_secs: u64, working_dir: Option<PathBuf>, max_output_bytes: usize) -> Self {
        Self {
            timeout_secs,
            working_dir,
            max_output_bytes,
        }
    }
}

/// Result of a [`TerminalBackend::execute`] call. Maps 1:1 to the `ToolOutput`
/// `ShellTool` returns — `content` is already stdout/stderr-formatted and
/// UTF-8-boundary-truncated.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ExecResult {
    pub success: bool,
    pub content: String,
    pub error: Option<String>,
}

impl ExecResult {
    pub fn new(success: bool, content: String, error: Option<String>) -> Self {
        Self {
            success,
            content,
            error,
        }
    }
}

/// Opaque handle to a backend snapshot (a Docker committed image tag, a Modal
/// snapshot id, a Daytona workspace id). Round-trips through the CLI.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct SnapshotHandle {
    pub id: String,
    pub backend: String,
}

impl SnapshotHandle {
    pub fn new(id: impl Into<String>, backend: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            backend: backend.into(),
        }
    }
}

// ─── TerminalBackend trait ───────────────────────────────────────────────────

/// Abstract terminal / shell-execution backend.
///
/// `ShellTool` holds an `Arc<dyn TerminalBackend>` and delegates execution
/// (after its own command-string safety pre-flight). Lifecycle methods
/// (`snapshot`/`restore`/`cleanup`) have safe default no-ops so stateless
/// backends (Local) need not implement them.
///
/// `cleanup(hibernate)` is the **single chokepoint** for session teardown
/// (evolution-plan §3.3):
/// - `hibernate=true`  → stop+keep (Modal snapshot+terminate, Daytona stop
///   FS-preserved, Docker stop+commit). Restorable via `restore`.
/// - `hibernate=false` → destroy (Docker `rm -f`, Modal terminate no-snapshot,
///   Daytona destroy). Local = no-op.
#[async_trait]
pub trait TerminalBackend: Send + Sync {
    /// Execute a command string, return formatted+truncated output.
    async fn execute(&self, command: &str, opts: &ExecOptions) -> Result<ExecResult>;

    /// Backend name (`"local"` / `"docker"` / `"modal"` / `"daytona"`).
    fn name(&self) -> &str;

    /// Whether this backend is usable right now (docker daemon up, creds
    /// present). Default: `true`.
    fn is_available(&self) -> bool {
        true
    }

    /// Whether snapshot/restore are real for this backend. Default `false`
    /// (LocalBackend — the local FS *is* the state, no snapshot needed).
    fn supports_snapshots(&self) -> bool {
        false
    }

    /// Snapshot the current session state. Default: unsupported.
    async fn snapshot(&self) -> Result<SnapshotHandle> {
        Err(OneAIError::Other(
            "terminal backend does not support snapshot".to_string(),
        ))
    }

    /// Restore from a snapshot handle. Default: unsupported.
    async fn restore(&self, _handle: &SnapshotHandle) -> Result<()> {
        Err(OneAIError::Other(
            "terminal backend does not support restore".to_string(),
        ))
    }

    /// Teardown. `hibernate=true` → stop+keep (restorable); `false` → destroy.
    /// Default: no-op (LocalBackend — nothing to clean up).
    async fn cleanup(&self, _hibernate: bool) -> Result<()> {
        Ok(())
    }
}

// ─── Output formatting (shared pure helper) ─────────────────────────────────

/// Format stdout/stderr into a single content string and truncate to
/// `max_output_bytes` on a UTF-8 char boundary (slicing on a byte index that
/// lands mid-codepoint panics on multibyte/CJK output — walk back to the
/// nearest boundary).
///
/// Shared by `LocalBackend` and `DockerTerminalBackend` so the truncation
/// regression stays fixed in one place.
pub(crate) fn format_and_truncate(stdout: &str, stderr: &str, max_output_bytes: usize) -> String {
    let content = if stderr.is_empty() {
        stdout.to_string()
    } else {
        format!("STDOUT:\n{}\nSTDERR:\n{}", stdout, stderr)
    };

    if content.len() > max_output_bytes {
        let mut end = max_output_bytes;
        while end > 0 && !content.is_char_boundary(end) {
            end -= 1;
        }
        let mut truncated = content[..end].to_string();
        truncated.push_str("\n... [output truncated due to size limit]");
        truncated
    } else {
        content
    }
}

/// Detect whether a sandbox-wrapped command's failure was a **sandbox policy
/// denial** (vs. a genuine command error), and return a clear, non-retryable
/// message when it was.
///
/// Seatbelt denies surface as `Operation not permitted` (EPERM); bwrap
/// ro-bind write attempts as `Read-only file system` (EROFS). Without this,
/// a denied write (`echo x > /opt/homebrew/...`) returned a generic
/// `Exit code: 1` — the agent loop re-prompted the model for self-correction,
/// the model retried the same doomed command, and the user saw "failure then
/// a second approval dialog" (issue #21). Surfacing a clear, non-retryable
/// denial stops the retry loop and tells the user exactly what the sandbox
/// blocked.
///
/// Only consulted when the command was sandbox-wrapped (`backend_name`
/// provided) — a bare `Permission denied` from an unwrapped command stays a
/// plain exit-code error (it's an OS permission issue, not sandbox policy).
pub(crate) fn sandbox_denial_message(
    backend_name: &str,
    stdout: &str,
    stderr: &str,
) -> Option<String> {
    let combined = format!("{stdout}\n{stderr}");
    // Match case-insensitively via lowercasing so "operation not permitted"
    // (some shells lower-case it) is caught too.
    let combined_l = combined.to_lowercase();
    let signature = if combined_l.contains("operation not permitted") {
        "operation not permitted"
    } else if combined_l.contains("read-only file system") {
        "read-only file system"
    } else {
        return None;
    };
    Some(format!(
        "Denied by {backend_name} sandbox policy: the sandbox blocked this operation \
         ({signature}). Writes outside the project/temp/HOME dirs and reads of ~/.ssh et al. \
         are not permitted. This is a security policy, not a transient failure — do NOT retry \
         the same command. Target a path inside the project, or ask the user to run it outside \
         the sandbox."
    ))
}

// ─── LocalBackend ────────────────────────────────────────────────────────────

/// Local terminal backend — the current `ShellTool` execution path, verbatim.
///
/// Holds an optional [`SandboxBackend`] for command wrapping (Seatbelt on
/// macOS, Docker-string on Linux, Regex pass-through). When `sandbox` is
/// `None`, runs the raw command (regex-only protection — the
/// `ShellTool::execute` pre-flight already ran).
///
/// `snapshot`/`restore`/`cleanup` are no-ops: the local filesystem *is* the
/// session state — nothing to snapshot or tear down.
pub struct LocalBackend {
    sandbox: Option<Arc<dyn SandboxBackend>>,
    #[allow(dead_code)]
    allowed_working_dirs: Vec<PathBuf>,
}

impl Default for LocalBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalBackend {
    /// Create a `LocalBackend` with no sandbox wrapping (regex-only baseline).
    pub fn new() -> Self {
        Self {
            sandbox: None,
            allowed_working_dirs: Vec::new(),
        }
    }

    /// Create a `LocalBackend` that wraps commands with the given sandbox
    /// backend before spawning (real process-level isolation).
    pub fn with_sandbox(
        sandbox: Arc<dyn SandboxBackend>,
        allowed_working_dirs: Vec<PathBuf>,
    ) -> Self {
        Self {
            sandbox: Some(sandbox),
            allowed_working_dirs,
        }
    }
}

#[async_trait]
impl TerminalBackend for LocalBackend {
    fn name(&self) -> &str {
        "local"
    }

    async fn execute(&self, command: &str, opts: &ExecOptions) -> Result<ExecResult> {
        // Determine the actual command to run — wrap with the sandbox backend
        // if one is configured. The working dir for wrapping comes from the
        // caller's ExecOptions (ShellTool builds it from its
        // allowed_working_dirs); default to "." when unspecified.
        //
        // `sandbox_name` is captured so a wrapped-command failure can be
        // checked against the sandbox-denial signature (issue #21) — surfacing
        // a clear non-retryable denial instead of a generic exit code.
        let (effective_command, sandbox_name) = if let Some(b) = &self.sandbox {
            let working_dir = opts
                .working_dir
                .as_deref()
                .unwrap_or_else(|| std::path::Path::new("."));
            let wrapped = b.wrap_command(command, working_dir)?;
            tracing::info!("ShellTool: command wrapped by {} sandbox backend", b.name());
            (wrapped.shell_command, Some(b.name().to_string()))
        } else {
            // No backend — just use the raw command (regex-only protection
            // already ran in ShellTool::execute).
            (command.to_string(), None)
        };

        // Resolve the shell based on the platform and (on Windows) whether
        // a POSIX `sh` is reachable. If the command is already wrapped by a
        // sandbox backend (contains "sandbox-exec" or "docker"), `sh` still
        // executes the wrapped command string directly on every platform.
        let (shell, shell_arg) = crate::tool_interfaces::resolve_shell();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(opts.timeout_secs),
            tokio::process::Command::new(shell)
                .arg(shell_arg)
                .arg(&effective_command)
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let content = format_and_truncate(&stdout, &stderr, opts.max_output_bytes);
                let error = if output.status.success() {
                    None
                } else if let Some(name) = &sandbox_name {
                    // Sandboxed failure — check for a sandbox policy denial
                    // first (issue #21); fall back to a plain exit code if
                    // the failure doesn't look like a sandbox denial.
                    sandbox_denial_message(name, &stdout, &stderr).or_else(|| {
                        Some(format!("Exit code: {}", output.status.code().unwrap_or(-1)))
                    })
                } else {
                    Some(format!("Exit code: {}", output.status.code().unwrap_or(-1)))
                };
                Ok(ExecResult {
                    success: output.status.success(),
                    content,
                    error,
                })
            }
            Ok(Err(e)) => Ok(ExecResult {
                success: false,
                content: String::new(),
                error: Some(format!("Failed to execute command: {}", e)),
            }),
            Err(_) => Ok(ExecResult {
                success: false,
                content: String::new(),
                error: Some(format!(
                    "Command timed out after {} seconds",
                    opts.timeout_secs
                )),
            }),
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_and_truncate_stdout_only() {
        let s = format_and_truncate("hello", "", 1000);
        assert_eq!(s, "hello");
    }

    #[test]
    fn test_format_and_truncate_with_stderr() {
        let s = format_and_truncate("out", "err", 1000);
        assert!(s.contains("STDOUT:\nout"));
        assert!(s.contains("STDERR:\nerr"));
    }

    #[test]
    fn test_format_and_truncate_empty_stderr_no_prefix() {
        // stderr empty → content is just stdout (no STDOUT:/STDERR: framing).
        let s = format_and_truncate("only stdout", "", 1000);
        assert_eq!(s, "only stdout");
        assert!(!s.contains("STDOUT:"));
    }

    #[test]
    fn test_format_and_truncate_oversized_ascii() {
        let big = "x".repeat(500);
        let s = format_and_truncate(&big, "", 100);
        assert!(s.ends_with("\n... [output truncated due to size limit]"));
        assert!(s.len() < big.len());
        // Truncated body is at most the cap (before the suffix).
        let body = s
            .strip_suffix("\n... [output truncated due to size limit]")
            .unwrap();
        assert!(body.len() <= 100);
    }

    #[test]
    fn test_format_and_truncate_multibyte_boundary() {
        // CJK chars are 3 bytes in UTF-8 — a naive byte slice at a non-boundary
        // panics. Walk-back must land on a char boundary.
        let big = "中".repeat(500); // 1500 bytes
        let s = format_and_truncate(&big, "", 100);
        assert!(s.ends_with("\n... [output truncated due to size limit]"));
        // Must not panic — already passed by reaching here.
        let body = s
            .strip_suffix("\n... [output truncated due to size limit]")
            .unwrap();
        assert!(body.len() <= 100);
        assert!(body.is_char_boundary(body.len())); // end of a valid string is always a boundary
    }

    #[test]
    fn test_exec_options_new() {
        let o = ExecOptions::new(30, None, 4096);
        assert_eq!(o.timeout_secs, 30);
        assert!(o.working_dir.is_none());
        assert_eq!(o.max_output_bytes, 4096);
    }

    #[test]
    fn test_snapshot_handle_new() {
        let h = SnapshotHandle::new("img:tag", "docker");
        assert_eq!(h.id, "img:tag");
        assert_eq!(h.backend, "docker");
    }

    struct NopBackend;
    #[async_trait]
    impl TerminalBackend for NopBackend {
        fn name(&self) -> &str {
            "nop"
        }
        async fn execute(&self, _command: &str, _opts: &ExecOptions) -> Result<ExecResult> {
            Ok(ExecResult::new(true, String::new(), None))
        }
    }

    #[tokio::test]
    async fn test_trait_defaults_snapshot_unsupported() {
        let b = NopBackend;
        assert!(!b.supports_snapshots());
        assert!(b.snapshot().await.is_err());
        assert!(b.restore(&SnapshotHandle::new("x", "nop")).await.is_err());
        // cleanup default = no-op Ok.
        assert!(b.cleanup(true).await.is_ok());
        assert!(b.cleanup(false).await.is_ok());
    }

    #[tokio::test]
    async fn test_local_backend_execute_echo() {
        // Behavior parity: LocalBackend (the ShellTool::new() default) runs
        // `echo hello` and returns success with the output.
        let b = LocalBackend::new();
        let opts = ExecOptions::new(30, None, 100_000);
        let res = b.execute("echo hello", &opts).await.unwrap();
        assert!(res.success, "echo should succeed: {:?}", res.error);
        assert!(res.content.contains("hello"), "content: {}", res.content);
    }

    #[tokio::test]
    async fn test_local_backend_execute_timeout() {
        let b = LocalBackend::new();
        let opts = ExecOptions::new(1, None, 100_000);
        let res = b.execute("sleep 5", &opts).await.unwrap();
        assert!(!res.success);
        let err = res.error.unwrap_or_default();
        assert!(err.contains("timed out"), "expected timeout, got: {err}");
    }

    #[tokio::test]
    async fn test_local_backend_execute_exit_code() {
        let b = LocalBackend::new();
        let opts = ExecOptions::new(30, None, 100_000);
        let res = b.execute("exit 7", &opts).await.unwrap();
        assert!(!res.success);
        let err = res.error.unwrap_or_default();
        assert!(err.contains("Exit code: 7"), "got: {err}");
    }

    #[tokio::test]
    async fn test_local_backend_name_and_availability() {
        let b = LocalBackend::new();
        assert_eq!(b.name(), "local");
        assert!(b.is_available());
        assert!(!b.supports_snapshots());
    }

    #[tokio::test]
    async fn test_local_backend_truncates_oversized_output() {
        let b = LocalBackend::new();
        let opts = ExecOptions::new(30, None, 50);
        // Print 200 bytes — must truncate to ~50 on a char boundary + suffix.
        // NOTE: avoid bash brace-expansion (`{1..200}`) — the CI's `/bin/sh` is
        // dash, which doesn't expand braces, so the printf would emit one byte
        // and never truncate. `/dev/zero` + `tr` is portable across sh impls.
        let res = b
            .execute("head -c 200 /dev/zero | tr '\\0' 'a'", &opts)
            .await
            .unwrap();
        assert!(res.content.contains("[output truncated due to size limit]"));
    }

    // ─── sandbox_denial_message (issue #21) ───────────────────────────────

    #[test]
    fn test_sandbox_denial_detects_operation_not_permitted() {
        // seatbelt EPERM on a file-write redirect: `sh: /opt/homebrew/x:
        // Operation not permitted`.
        let msg = sandbox_denial_message(
            "seatbelt",
            "",
            "sh: /opt/homebrew/oneai_sb_test.txt: Operation not permitted",
        );
        let msg = msg.expect("seatbelt EPERM must be detected");
        assert!(msg.contains("Denied by seatbelt sandbox policy"));
        assert!(msg.contains("operation not permitted"));
        assert!(msg.contains("do NOT retry"));
    }

    #[test]
    fn test_sandbox_denial_detects_readonly_filesystem() {
        // bwrap ro-bind write: `Read-only file system` (EROFS).
        let msg = sandbox_denial_message("bwrap", "", "sh: /usr/local/x: Read-only file system")
            .expect("bwrap EROFS must be detected");
        assert!(msg.contains("Denied by bwrap sandbox policy"));
        assert!(msg.contains("read-only file system"));
    }

    #[test]
    fn test_sandbox_denial_case_insensitive() {
        // Some shells lowercase the errno string.
        assert!(sandbox_denial_message("seatbelt", "", "x: operation not permitted").is_some());
    }

    #[test]
    fn test_sandbox_denial_ignores_unrelated_failure() {
        // A genuine command error (not a sandbox denial) → no special
        // message; the caller falls back to the plain exit code.
        assert!(sandbox_denial_message("seatbelt", "command not found", "").is_none());
        assert!(sandbox_denial_message("bwrap", "", "some other EPERM-less error text").is_none());
    }

    #[test]
    fn test_sandbox_denial_checks_stdout_too() {
        // seatbelt sometimes surfaces the EPERM on stdout (e.g. `ls ~/.ssh`
        // prints "ls: ...: Operation not permitted" to stderr, but a redirect
        // chain can route it to stdout). Both are scanned.
        assert!(sandbox_denial_message("seatbelt", "Operation not permitted", "").is_some());
    }

    /// Issue #21 end-to-end: a seatbelt-denied write under the real macOS
    /// sandbox must surface the clear denial message (not a generic exit
    /// code), so the model doesn't loop-retry the doomed command. macOS-only
    /// (needs `sandbox-exec`); skipped elsewhere.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn test_local_backend_surfaces_seatbelt_denial() {
        if !std::path::Path::new("/usr/bin/sandbox-exec").exists() {
            eprintln!("sandbox-exec not present; skipping");
            return;
        }
        // Build a LocalBackend wired with the real SeatbeltBackend (the same
        // default CodingPack uses). Project dir = a temp dir (allowed write
        // root). The write target is a path the OS lets the user write but
        // the seatbelt does NOT allow (outside project/tmp/HOME): /opt/homebrew
        // on Apple Silicon is user-owned, so the denial is the sandbox's, not
        // the OS's — proving the message is the sandbox-denial path.
        let tmp = std::env::temp_dir().join(format!(
            "oneai_seatbelt_denial_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let seatbelt = crate::sandbox::SeatbeltBackend::coding_defaults(&tmp);
        if !seatbelt.is_available() {
            let _ = std::fs::remove_dir_all(&tmp);
            eprintln!("seatbelt not available; skipping");
            return;
        }
        let backend = LocalBackend::with_sandbox(Arc::new(seatbelt), Vec::new());
        let opts = ExecOptions::new(30, Some(std::path::PathBuf::from(&tmp)), 100_000);
        let target = "/opt/homebrew/oneai_seatbelt_denial_probe.txt";
        let res = backend
            .execute(&format!("echo probe > {target}"), &opts)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_file(target);
        assert!(
            !res.success,
            "the seatbelt should have denied the write; got success. content={}",
            res.content
        );
        let err = res.error.expect("denied command must have an error");
        assert!(
            err.contains("Denied by seatbelt sandbox policy"),
            "expected the clear sandbox-denial message, got: {err}"
        );
        assert!(err.contains("do NOT retry"));
    }
}
