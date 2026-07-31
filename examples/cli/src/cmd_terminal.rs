//! Terminal command — `TerminalBackend` management (Phase 3.3).
//!
//! `oneai terminal list/exec/snapshot/restore/cleanup` drives the
//! `TerminalBackend` trait out of band from the agent loop: list available
//! backends, run a one-off command through a chosen backend, and exercise
//! the snapshot/restore/cleanup(hibernate) lifecycle. Mirrors the gateway
//! and cron commands — the backends sit below `oneai-app`, constructed
//! directly here by name.

use std::sync::Arc;

use oneai_tool::terminal::docker::DockerTerminalBackend;
use oneai_tool::terminal::{ExecOptions, LocalBackend, SnapshotHandle, TerminalBackend};

/// Build a backend by name. `local` is always available; `docker` requires
/// the docker binary; `modal`/`daytona` require their feature flags (the CLI
/// enables both on the `oneai-tool` dependency, so both are compiled in).
fn build_backend(name: &str) -> Result<Arc<dyn TerminalBackend>, String> {
    match name {
        "local" => Ok(Arc::new(LocalBackend::new())),
        "docker" => {
            let b = DockerTerminalBackend::coding_defaults(std::path::PathBuf::from("."));
            if !b.is_available() {
                return Err("docker binary not found on host".to_string());
            }
            Ok(Arc::new(b))
        }
        "modal" => {
            let key = std::env::var("MODAL_TOKEN").ok();
            let app = std::env::var("MODAL_APP").unwrap_or_else(|_| "oneai-terminal".to_string());
            Ok(Arc::new(oneai_tool::ModalBackend::new(app, key)))
        }
        "daytona" => {
            let key = std::env::var("DAYTONA_API_KEY").ok();
            let host = std::env::var("DAYTONA_HOST").unwrap_or_else(|_| "".to_string());
            Ok(Arc::new(oneai_tool::DaytonaBackend::new(host, key)))
        }
        other => Err(format!("unknown backend: {other}")),
    }
}

pub fn cmd_terminal_list() {
    println!("available terminal backends:");
    println!("  local   — always available (current behavior, verbatim)");
    if DockerTerminalBackend::coding_defaults(std::path::PathBuf::from(".")).is_available() {
        println!("  docker  — long-lived container lifecycle (snapshot/restore/cleanup)");
    } else {
        println!("  docker  — (docker binary not found; install docker to enable)");
    }
    println!("  modal   — serverless HTTP terminal (MODAL_TOKEN / MODAL_APP env)");
    println!("  daytona — serverless HTTP terminal (DAYTONA_API_KEY / DAYTONA_HOST env)");
}

pub fn cmd_terminal_exec(backend: &str, command: &str, timeout: u64, max_output: usize) {
    let b = match build_backend(backend) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };
    if !b.is_available() {
        eprintln!("backend `{}` is not available on this host", backend);
        std::process::exit(1);
    }
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let opts = ExecOptions::new(timeout, None, max_output);
    match rt.block_on(b.execute(command, &opts)) {
        Ok(res) => {
            print!("{}", res.content);
            if let Some(e) = res.error {
                eprintln!("\n[error] {e}");
            }
            if !res.success {
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("execute failed: {e}");
            std::process::exit(1);
        }
    }
}

pub fn cmd_terminal_snapshot(backend: &str) {
    let b = match build_backend(backend) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };
    if !b.supports_snapshots() {
        eprintln!("backend `{}` does not support snapshots", backend);
        std::process::exit(1);
    }
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    match rt.block_on(b.snapshot()) {
        Ok(handle) => {
            println!("snapshot id: {}  (backend: {})", handle.id, handle.backend);
        }
        Err(e) => {
            eprintln!("snapshot failed: {e}");
            std::process::exit(1);
        }
    }
}

pub fn cmd_terminal_restore(backend: &str, id: &str) {
    let b = match build_backend(backend) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let handle = SnapshotHandle::new(id, backend);
    match rt.block_on(b.restore(&handle)) {
        Ok(()) => println!("restored from {id}"),
        Err(e) => {
            eprintln!("restore failed: {e}");
            std::process::exit(1);
        }
    }
}

pub fn cmd_terminal_cleanup(backend: &str, hibernate: bool) {
    let b = match build_backend(backend) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    match rt.block_on(b.cleanup(hibernate)) {
        Ok(()) => {
            if hibernate {
                println!("hibernated (state preserved, restorable)");
            } else {
                println!("destroyed");
            }
        }
        Err(e) => {
            eprintln!("cleanup failed: {e}");
            std::process::exit(1);
        }
    }
}
