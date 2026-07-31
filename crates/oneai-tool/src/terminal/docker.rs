//! `DockerTerminalBackend` — a `TerminalBackend` backed by a long-lived
//! Docker container (Phase 3.3, Stage C).
//!
//! Distinct from [`crate::sandbox::DockerBackend`] (in `sandbox.rs`), which
//! only string-wraps a `docker run --rm` per call with no persistence. This
//! backend owns a **long-lived container** and does real lifecycle:
//!
//! - [`execute`](DockerTerminalBackend::execute): `docker exec` against the
//!   running container (lazily created on first call).
//! - [`snapshot`](DockerTerminalBackend::snapshot): `docker commit` —
//!   captures the container's FS layer into a restorable image tag.
//! - [`restore`](DockerTerminalBackend::restore): recreate the container
//!   from a committed image tag (stop + rm + create + start).
//! - [`cleanup`](DockerTerminalBackend::cleanup): the **single teardown
//!   chokepoint**. `hibernate=true` → `docker stop` (container + FS layer
//!   preserved, restorable via `docker start`); `hibernate=false` →
//!   `docker rm -f` (destroy).

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use oneai_core::error::{OneAIError, Result};

use super::{format_and_truncate, ExecOptions, ExecResult, SnapshotHandle, TerminalBackend};

// ─── Argv construction (pure, unit-testable without a daemon) ────────────────

/// Build the `docker create` argv for a long-lived container whose only job
/// is to stay alive (`tail -f /dev/null`) so `docker exec` can run against it.
pub fn build_create_args(
    name: &str,
    image: &str,
    mount_dirs: &[PathBuf],
    allow_network: bool,
) -> Vec<String> {
    let mut args = vec!["create".to_string(), "--name".to_string(), name.to_string()];
    for dir in mount_dirs {
        let d = dir.to_string_lossy();
        args.push("-v".to_string());
        args.push(format!("{}:{}", d, d));
    }
    if !allow_network {
        args.push("--network".to_string());
        args.push("none".to_string());
    }
    args.push(image.to_string());
    args.push("tail".to_string());
    args.push("-f".to_string());
    args.push("/dev/null".to_string());
    args
}

/// Build the `docker exec` argv for running a command string in the container.
pub fn build_exec_args(name: &str, command: &str) -> Vec<String> {
    vec![
        "exec".to_string(),
        name.to_string(),
        "sh".to_string(),
        "-c".to_string(),
        command.to_string(),
    ]
}

pub fn build_start_args(name: &str) -> Vec<String> {
    vec!["start".to_string(), name.to_string()]
}

pub fn build_stop_args(name: &str) -> Vec<String> {
    vec!["stop".to_string(), name.to_string()]
}

pub fn build_rm_args(name: &str) -> Vec<String> {
    vec!["rm".to_string(), "-f".to_string(), name.to_string()]
}

pub fn build_commit_args(name: &str, tag: &str) -> Vec<String> {
    vec!["commit".to_string(), name.to_string(), tag.to_string()]
}

/// Build the `docker create` argv for restoring from a committed image tag.
pub fn build_restore_create_args(
    name: &str,
    image_tag: &str,
    mount_dirs: &[PathBuf],
    allow_network: bool,
) -> Vec<String> {
    build_create_args(name, image_tag, mount_dirs, allow_network)
}

// ─── Docker binary location ──────────────────────────────────────────────────

/// Locate the `docker` binary on the host. Checks common install paths then
/// `PATH`. Returns `None` if not found (so `is_available` is false).
fn docker_binary() -> Option<PathBuf> {
    for candidate in [
        PathBuf::from("/usr/bin/docker"),
        PathBuf::from("/usr/local/bin/docker"),
        PathBuf::from("/opt/homebrew/bin/docker"),
    ] {
        if candidate.exists() {
            return Some(candidate);
        }
    }
    // Fall back to PATH lookup via `which`-style: spawn `docker --version`.
    // (Cheaper to check the common paths first; this is the fallback.)
    which_docker()
}

#[cfg(unix)]
fn which_docker() -> Option<PathBuf> {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("command -v docker 2>/dev/null")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

#[cfg(not(unix))]
fn which_docker() -> Option<PathBuf> {
    // On Windows, assume `docker` is on PATH if `docker --version` runs.
    let out = std::process::Command::new("docker")
        .arg("--version")
        .output()
        .ok()?;
    if out.status.success() {
        Some(PathBuf::from("docker"))
    } else {
        None
    }
}

// ─── DockerTerminalBackend ───────────────────────────────────────────────────

/// A `TerminalBackend` backed by a long-lived Docker container.
pub struct DockerTerminalBackend {
    container_name: String,
    image: String,
    mount_dirs: Vec<PathBuf>,
    allow_network: bool,
    /// Lazily set after the first successful `create`+`start`. The
    /// `ensure_started` fallback also handles the missing-container case, so
    /// this flag is an optimization (skip the probe), not a correctness gate.
    started: AtomicBool,
}

impl DockerTerminalBackend {
    /// Create a handle to a Docker-backed terminal. The container is created
    /// lazily on the first `execute` (or eagerly via `ensure_started`).
    pub fn new(
        container_name: impl Into<String>,
        image: impl Into<String>,
        mount_dirs: Vec<PathBuf>,
        allow_network: bool,
    ) -> Self {
        Self {
            container_name: container_name.into(),
            image: image.into(),
            mount_dirs,
            allow_network,
            started: AtomicBool::new(false),
        }
    }

    /// Coding-task defaults: a stable container name, `oneai-sandbox:latest`,
    /// mount + network allowed.
    pub fn coding_defaults(project_dir: PathBuf) -> Self {
        Self::new(
            "oneai-terminal",
            "oneai-sandbox:latest",
            vec![project_dir],
            true,
        )
    }

    fn bin(&self) -> Result<PathBuf> {
        docker_binary()
            .ok_or_else(|| OneAIError::Other("docker binary not found on host".to_string()))
    }

    /// Ensure the container exists and is running. Probes `docker start`
    /// first (a no-op if already running); if the container is missing, creates
    /// it then starts it. Robust to restarts and to a freshly-pulled image.
    async fn ensure_started(&self) -> Result<()> {
        if self.started.load(Ordering::Relaxed) {
            return Ok(());
        }
        let bin = self.bin()?;
        // Try start (idempotent on a running container).
        let start = run_docker(&bin, &build_start_args(&self.container_name)).await;
        match start {
            Ok(_) => {
                self.started.store(true, Ordering::Relaxed);
                Ok(())
            }
            Err(_) => {
                // Container missing — create then start.
                let create_args = build_create_args(
                    &self.container_name,
                    &self.image,
                    &self.mount_dirs,
                    self.allow_network,
                );
                run_docker(&bin, &create_args).await?;
                run_docker(&bin, &build_start_args(&self.container_name)).await?;
                self.started.store(true, Ordering::Relaxed);
                Ok(())
            }
        }
    }
}

/// Run `docker <args>`, capturing output. Returns the `Output` on success
/// (exit 0), else an error carrying stderr.
async fn run_docker(bin: &std::path::Path, args: &[String]) -> Result<std::process::Output> {
    let out = tokio::process::Command::new(bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| OneAIError::Other(format!("docker spawn failed: {e}")))?;
    if out.status.success() {
        Ok(out)
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        Err(OneAIError::Other(format!(
            "docker {} failed: {}",
            args.join(" "),
            stderr.trim()
        )))
    }
}

#[async_trait]
impl TerminalBackend for DockerTerminalBackend {
    fn name(&self) -> &str {
        "docker"
    }

    fn is_available(&self) -> bool {
        docker_binary().is_some()
    }

    fn supports_snapshots(&self) -> bool {
        true
    }

    async fn execute(&self, command: &str, opts: &ExecOptions) -> Result<ExecResult> {
        self.ensure_started().await?;
        let bin = self.bin()?;
        let args = build_exec_args(&self.container_name, command);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(opts.timeout_secs),
            tokio::process::Command::new(&bin)
                .args(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let content = format_and_truncate(&stdout, &stderr, opts.max_output_bytes);
                Ok(ExecResult {
                    success: output.status.success(),
                    content,
                    error: if output.status.success() {
                        None
                    } else {
                        Some(format!("Exit code: {}", output.status.code().unwrap_or(-1)))
                    },
                })
            }
            Ok(Err(e)) => Ok(ExecResult {
                success: false,
                content: String::new(),
                error: Some(format!("Failed to execute docker exec: {}", e)),
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

    async fn snapshot(&self) -> Result<SnapshotHandle> {
        self.ensure_started().await?;
        let bin = self.bin()?;
        // Commit the running container's FS layer into a restorable image tag.
        // Deterministic tag would need a timestamp; the caller (CLI) supplies
        // none, so derive from the container name + a static suffix. Multiple
        // snapshots overwrite the same tag — acceptable for the v1 seam
        // (preserve-the-latest); richer tagging is a follow-up.
        let tag = format!("{}:snapshot", self.container_name);
        let _ = run_docker(&bin, &build_commit_args(&self.container_name, &tag)).await?;
        Ok(SnapshotHandle::new(tag, self.name()))
    }

    async fn restore(&self, handle: &SnapshotHandle) -> Result<()> {
        let bin = self.bin()?;
        // Discard the current container, recreate from the committed image.
        let _ = run_docker(&bin, &build_stop_args(&self.container_name)).await;
        let _ = run_docker(&bin, &build_rm_args(&self.container_name)).await;
        let create_args = build_restore_create_args(
            &self.container_name,
            &handle.id,
            &self.mount_dirs,
            self.allow_network,
        );
        run_docker(&bin, &create_args).await?;
        run_docker(&bin, &build_start_args(&self.container_name)).await?;
        self.started.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn cleanup(&self, hibernate: bool) -> Result<()> {
        let bin = match self.bin() {
            Ok(b) => b,
            Err(_) => return Ok(()), // nothing to clean if docker is gone
        };
        if hibernate {
            // Stop + keep: container + FS layer preserved, restorable via start.
            let _ = run_docker(&bin, &build_stop_args(&self.container_name)).await;
            self.started.store(false, Ordering::Relaxed);
        } else {
            // Destroy.
            let _ = run_docker(&bin, &build_rm_args(&self.container_name)).await;
            self.started.store(false, Ordering::Relaxed);
        }
        Ok(())
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_create_args_basic() {
        let args = build_create_args("c1", "img:latest", &[], true);
        assert_eq!(args[0], "create");
        assert!(args.contains(&"--name".to_string()));
        assert!(args.contains(&"c1".to_string()));
        assert!(args.contains(&"img:latest".to_string()));
        // tail -f /dev/null keeps the container alive
        assert!(args.contains(&"tail".to_string()));
        assert!(args.contains(&"/dev/null".to_string()));
        // network allowed → no --network none
        assert!(!args.contains(&"--network".to_string()));
    }

    #[test]
    fn test_build_create_args_no_network_mounts() {
        let dir = PathBuf::from("/proj");
        let args = build_create_args("c1", "img", std::slice::from_ref(&dir), false);
        assert!(args.contains(&"-v".to_string()));
        assert!(args.contains(&"/proj:/proj".to_string()));
        // network disabled → --network none present
        let i = args.iter().position(|a| a == "--network").unwrap();
        assert_eq!(args[i + 1], "none");
    }

    #[test]
    fn test_build_exec_args() {
        let args = build_exec_args("c1", "echo hi");
        assert_eq!(args, vec!["exec", "c1", "sh", "-c", "echo hi"]);
    }

    #[test]
    fn test_build_lifecycle_args() {
        assert_eq!(build_start_args("c1"), vec!["start", "c1"]);
        assert_eq!(build_stop_args("c1"), vec!["stop", "c1"]);
        assert_eq!(build_rm_args("c1"), vec!["rm", "-f", "c1"]);
        assert_eq!(
            build_commit_args("c1", "img:tag"),
            vec!["commit", "c1", "img:tag"]
        );
    }

    #[test]
    fn test_restore_create_uses_snapshot_tag() {
        let dir = PathBuf::from("/p");
        let args = build_restore_create_args("c1", "img:snap", &[dir], true);
        // Image position is the committed tag, not the original image.
        assert!(args.contains(&"img:snap".to_string()));
        // Restore create keeps the alive-entrypoint (tail -f /dev/null).
        assert!(args.contains(&"tail".to_string()));
    }

    #[test]
    fn test_docker_backend_name_and_supports_snapshots() {
        let b = DockerTerminalBackend::coding_defaults(PathBuf::from("/proj"));
        assert_eq!(b.name(), "docker");
        assert!(b.supports_snapshots());
    }

    #[test]
    fn test_is_available_does_not_panic() {
        // Whether or not docker is installed, this must not panic / hang.
        let b = DockerTerminalBackend::coding_defaults(PathBuf::from("/proj"));
        let _ = b.is_available();
    }
}
