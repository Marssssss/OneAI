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

/// Resolve a path the way file tools should open it. Absolute paths pass
/// through unchanged. Relative paths resolve against the active session
/// workspace when one is bound, NOT the process cwd — otherwise a background
/// sub-agent (which never sets `active_cwd` itself but inherits the parent
/// turn's) writes its files to the binary's launch dir instead of the
/// user-picked workspace, diverging from the parent. When no workspace is
/// bound (legacy in-process run), the path is returned as-is so the caller's
/// own `std::env::current_dir()`-relative behavior is preserved.
pub fn resolve(path: &str) -> String {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        return path.to_string();
    }
    match active_cwd() {
        Some(w) => w.join(p).to_string_lossy().into_owned(),
        None => path.to_string(),
    }
}
