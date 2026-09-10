//! Docker implementation of [`ContainerRunner`].
//!
//! Structure mirrors `oneai-tool/src/terminal/docker.rs`: **pure argv
//! builders** (`build_*_args`) that are golden-unit-tested without any IO,
//! plus a thin async execution wrapper around the docker CLI.

use std::time::Duration;

use async_trait::async_trait;

use crate::error::{OrchestratorError, Result};
use crate::runner::{ContainerHandle, ContainerRunner, SessionSpec};

// ─── Pure argv builders (no IO, golden-tested) ────────────────────────────────

/// `docker run -d --name <name> -v <state>:/home/oneai/.oneai
///  -v <ws>:/workspace [-v <cfg>:/home/oneai/.oneai/config.toml:ro]
///  -p <bind_host>:0:<port> [-e K=V]... --restart no <image>
///  oneai web --no-open --host 0.0.0.0 --port <port>`
pub fn build_spawn_args(spec: &SessionSpec) -> Vec<String> {
    let mut args = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        spec.container_name(),
    ];
    args.push("-v".into());
    args.push(format!("{}:/home/oneai/.oneai", spec.state_volume));
    args.push("-v".into());
    args.push(format!("{}:/workspace", spec.workspace_volume));
    if let Some(cfg) = &spec.provider_config {
        args.push("-v".into());
        args.push(format!(
            "{}:/home/oneai/.oneai/config.toml:ro",
            cfg.display()
        ));
    }
    // Dynamic host port, published on bind_host only (D7 mitigation).
    args.push("-p".into());
    args.push(format!("{}:0:{}", spec.bind_host, spec.container_port));
    for (k, v) in &spec.env {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    // The orchestrator owns restart policy (FSM), not docker.
    args.push("--restart".into());
    args.push("no".into());
    args.push(spec.image.clone());
    // Explicit CMD (don't rely on the image default — container_port is
    // configurable): `oneai web` serves HTTP+WS(/ws)+SPA on one port.
    args.extend([
        "oneai".into(),
        "web".into(),
        "--no-open".into(),
        "--host".into(),
        "0.0.0.0".into(),
        "--port".into(),
        spec.container_port.to_string(),
    ]);
    args
}

/// `docker start <name>`
pub fn build_start_args(container_name: &str) -> Vec<String> {
    vec!["start".into(), container_name.into()]
}

/// `docker stop -t 10 <name>` (10s grace before SIGKILL).
pub fn build_stop_args(container_name: &str) -> Vec<String> {
    vec![
        "stop".into(),
        "-t".into(),
        "10".into(),
        container_name.into(),
    ]
}

/// `docker rm -f <name>`
pub fn build_rm_args(container_name: &str) -> Vec<String> {
    vec!["rm".into(), "-f".into(), container_name.into()]
}

/// `docker volume create <vol>` (idempotent).
pub fn build_volume_create_args(volume: &str) -> Vec<String> {
    vec!["volume".into(), "create".into(), volume.into()]
}

/// `docker volume rm -f <vols...>`
pub fn build_volume_rm_args(volumes: &[String]) -> Vec<String> {
    let mut args = vec!["volume".into(), "rm".into(), "-f".into()];
    args.extend(volumes.iter().cloned());
    args
}

/// `docker inspect --format '{{(index (index .NetworkSettings.Ports "<port>/tcp") 0).HostPort}}' <name>`
pub fn build_inspect_port_args(container_name: &str, container_port: u16) -> Vec<String> {
    vec![
        "inspect".into(),
        "--format".into(),
        format!(
            "{{{{(index (index .NetworkSettings.Ports \"{container_port}/tcp\") 0).HostPort}}}}"
        ),
        container_name.into(),
    ]
}

/// `docker inspect --format '{{.State.Status}}' <name>`
pub fn build_inspect_state_args(container_name: &str) -> Vec<String> {
    vec![
        "inspect".into(),
        "--format".into(),
        "{{.State.Status}}".into(),
        container_name.into(),
    ]
}

/// `docker commit <name> <tag>`
pub fn build_commit_args(container_name: &str, tag: &str) -> Vec<String> {
    vec!["commit".into(), container_name.into(), tag.into()]
}

// ─── Execution wrapper ────────────────────────────────────────────────────────

/// Docker CLI container runner.
#[derive(Debug, Clone)]
pub struct DockerRunner {
    bin: String,
    /// Host interface to TCP-probe in `health` (matches the published-port
    /// bind host; the orchestrator runs on the same machine).
    probe_host: String,
}

impl DockerRunner {
    /// New runner using the `docker` binary.
    pub fn new() -> Self {
        Self {
            bin: "docker".into(),
            probe_host: "127.0.0.1".into(),
        }
    }

    /// New runner with an explicit binary path/name and health-probe host.
    pub fn with_bin(bin: impl Into<String>, probe_host: impl Into<String>) -> Self {
        Self {
            bin: bin.into(),
            probe_host: probe_host.into(),
        }
    }

    /// Run `docker <args>`, returning trimmed stdout. Err carries stderr.
    async fn run(&self, args: &[String]) -> Result<String> {
        let out = tokio::process::Command::new(&self.bin)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(|e| OrchestratorError::Runner(format!("{}: {e}", self.bin)))?;
        if !out.status.success() {
            return Err(OrchestratorError::Runner(format!(
                "docker {} failed ({}): {}",
                args.first().map(|s| s.as_str()).unwrap_or("?"),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// `docker volume create` — idempotent (existing volume returns its name).
    async fn ensure_volume(&self, volume: &str) -> Result<()> {
        self.run(&build_volume_create_args(volume)).await?;
        Ok(())
    }

    /// Poll `docker inspect` for the dynamic host port after start. The
    /// mapping appears only once the container is up; a container that dies
    /// immediately never yields one (→ `no_host_port` failure).
    async fn poll_host_port(&self, name: &str, container_port: u16) -> Result<u16> {
        let args = build_inspect_port_args(name, container_port);
        for attempt in 0..10 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            if let Ok(s) = self.run(&args).await {
                if let Ok(port) = s.trim().parse::<u16>() {
                    return Ok(port);
                }
            }
        }
        Err(OrchestratorError::Runner(format!(
            "no_host_port: container {name} did not publish {container_port}/tcp"
        )))
    }

    /// `docker inspect` state status ("running"/"exited"/…). Err if the
    /// container is gone.
    async fn inspect_state(&self, name: &str) -> Result<String> {
        self.run(&build_inspect_state_args(name)).await
    }
}

impl Default for DockerRunner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ContainerRunner for DockerRunner {
    async fn spawn(&self, spec: &SessionSpec) -> Result<ContainerHandle> {
        self.ensure_volume(&spec.state_volume).await?;
        self.ensure_volume(&spec.workspace_volume).await?;
        let name = spec.container_name();
        // Tolerate a leftover container of the same name (crash remnant):
        // remove it first — its volumes (the session's data) survive.
        if self.inspect_state(&name).await.is_ok() {
            self.run(&build_rm_args(&name)).await?;
        }
        let container_id = self.run(&build_spawn_args(spec)).await?;
        let host_port = match self.poll_host_port(&name, spec.container_port).await {
            Ok(p) => p,
            Err(e) => {
                // Don't leak a half-started container.
                let _ = self.run(&build_rm_args(&name)).await;
                return Err(e);
            }
        };
        Ok(ContainerHandle {
            container_id,
            container_name: name,
            host_port,
        })
    }

    async fn stop(&self, handle: &ContainerHandle) -> Result<()> {
        self.run(&build_stop_args(&handle.container_name)).await?;
        Ok(())
    }

    async fn start(&self, handle: &ContainerHandle) -> Result<ContainerHandle> {
        self.run(&build_start_args(&handle.container_name)).await?;
        // Dynamic port mapping changes across stop/start — re-inspect.
        let host_port = self.poll_host_port_by_name(&handle.container_name).await?;
        Ok(ContainerHandle {
            container_id: handle.container_id.clone(),
            container_name: handle.container_name.clone(),
            host_port,
        })
    }

    async fn health(&self, handle: &ContainerHandle) -> Result<bool> {
        match self.inspect_state(&handle.container_name).await {
            Ok(s) if s.trim() == "running" => {}
            _ => return Ok(false),
        }
        // Container process alive ≠ engine listening; TCP-probe the port.
        let addr = format!("{}:{}", self.probe_host, handle.host_port);
        let probe = tokio::time::timeout(
            Duration::from_millis(500),
            tokio::net::TcpStream::connect(&addr),
        )
        .await;
        Ok(matches!(probe, Ok(Ok(_))))
    }

    async fn commit(&self, handle: &ContainerHandle, tag: &str) -> Result<()> {
        self.run(&build_commit_args(&handle.container_name, tag))
            .await?;
        Ok(())
    }

    async fn destroy(&self, handle: &ContainerHandle, remove_volumes: bool) -> Result<()> {
        // rm -f is tolerated when already gone.
        let _ = self.run(&build_rm_args(&handle.container_name)).await;
        if remove_volumes {
            // Volume names follow the canonical convention; derive from the
            // container name (oneai-orch-<id> → -state/-ws).
            let base = &handle.container_name;
            let vols = vec![format!("{base}-state"), format!("{base}-ws")];
            let _ = self.run(&build_volume_rm_args(&vols)).await;
        }
        Ok(())
    }
}

impl DockerRunner {
    /// Re-inspect the published host port without knowing container_port:
    /// read the whole `NetworkSettings.Ports` first-match HostPort.
    async fn poll_host_port_by_name(&self, name: &str) -> Result<u16> {
        let args = vec![
            "inspect".into(),
            "--format".into(),
            "{{range $p, $b := .NetworkSettings.Ports}}{{if $b}}{{(index $b 0).HostPort}}{{break}}{{end}}{{end}}"
                .into(),
            name.into(),
        ];
        for attempt in 0..10 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            if let Ok(s) = self.run(&args).await {
                if let Ok(port) = s.trim().parse::<u16>() {
                    return Ok(port);
                }
            }
        }
        Err(OrchestratorError::Runner(format!(
            "no_host_port: container {name} has no published port after start"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn spec() -> SessionSpec {
        SessionSpec {
            session_id: "s1".into(),
            image: "oneai-engine:mvs1".into(),
            state_volume: "oneai-orch-s1-state".into(),
            workspace_volume: "oneai-orch-s1-ws".into(),
            env: vec![("OPENAI_API_KEY".into(), "sk-x".into())],
            bind_host: "127.0.0.1".into(),
            container_port: 8787,
            provider_config: Some("/home/u/.oneai/config.toml".into()),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn spawn_args_golden() {
        let args = build_spawn_args(&spec());
        assert_eq!(
            args,
            vec![
                "run",
                "-d",
                "--name",
                "oneai-orch-s1",
                "-v",
                "oneai-orch-s1-state:/home/oneai/.oneai",
                "-v",
                "oneai-orch-s1-ws:/workspace",
                "-v",
                "/home/u/.oneai/config.toml:/home/oneai/.oneai/config.toml:ro",
                "-p",
                "127.0.0.1:0:8787",
                "-e",
                "OPENAI_API_KEY=sk-x",
                "--restart",
                "no",
                "oneai-engine:mvs1",
                "oneai",
                "web",
                "--no-open",
                "--host",
                "0.0.0.0",
                "--port",
                "8787",
            ]
        );
    }

    #[test]
    fn spawn_args_minimal_no_config_no_env() {
        let mut s = spec();
        s.provider_config = None;
        s.env = vec![];
        let args = build_spawn_args(&s);
        assert!(!args.iter().any(|a| a.ends_with("config.toml:ro")));
        assert!(!args.contains(&"-e".to_string()));
    }

    #[test]
    fn lifecycle_args_golden() {
        assert_eq!(build_start_args("c"), vec!["start", "c"]);
        assert_eq!(build_stop_args("c"), vec!["stop", "-t", "10", "c"]);
        assert_eq!(build_rm_args("c"), vec!["rm", "-f", "c"]);
        assert_eq!(build_volume_create_args("v"), vec!["volume", "create", "v"]);
        assert_eq!(
            build_volume_rm_args(&["a".into(), "b".into()]),
            vec!["volume", "rm", "-f", "a", "b"]
        );
        assert_eq!(build_commit_args("c", "t:1"), vec!["commit", "c", "t:1"]);
    }

    #[test]
    fn inspect_args_golden() {
        assert_eq!(
            build_inspect_port_args("c", 8787),
            vec![
                "inspect",
                "--format",
                "{{(index (index .NetworkSettings.Ports \"8787/tcp\") 0).HostPort}}",
                "c"
            ]
        );
        assert_eq!(
            build_inspect_state_args("c"),
            vec!["inspect", "--format", "{{.State.Status}}", "c"]
        );
    }
}
