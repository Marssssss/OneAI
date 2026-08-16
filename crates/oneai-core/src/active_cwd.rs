//! Process-scoped "active working directory" — the per-session workspace the
//! agent operates in (deepseek-harness parity). The OneAI app-server runs one
//! active turn at a time, so a process-global cell (set at turn start from
//! `conversation.metadata["workspace"]`) lets the shared, baked-at-construction
//! tools — the shell `ExecOptions.working_dir`, the Seatbelt write-allowlist,
//! `SandboxedFileOps.allowed_roots`, and the `EnvironmentInfoSource` context
//! ("Working Directory:") — all resolve to the session's workspace without a
//! per-invocation context plumbing through the `Tool` trait.
//!
//! `None` ⇐ no workspace bound (the app-global cwd, the legacy default).

use std::path::PathBuf;

static ACTIVE_CWD: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

/// Set the active session working directory. Called at turn start from the
/// session's bound workspace (`metadata["workspace"]`); `None` clears it
/// (reverts to the process cwd / baked tool defaults).
pub fn set_active_cwd(path: Option<PathBuf>) {
    if let Ok(mut g) = ACTIVE_CWD.write() {
        // Diagnostic: log so the sidecar log/terminal confirms whether the
        // session workspace reaches the runtime cwd layer.
        tracing::info!(
            "active_cwd set to: {}",
            path.as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<none>".into())
        );
        *g = path;
    }
}

/// Read the active session working directory, if any. Tools / context sources
/// call this and fall back to `std::env::current_dir()` / their baked default
/// when `None`.
pub fn active_cwd() -> Option<PathBuf> {
    ACTIVE_CWD.read().ok().and_then(|g| g.clone())
}
