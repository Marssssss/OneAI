//! Orchestrator command — run the cloud session control plane (MVS2), or
//! drive a running one as a client (`create/list/status/destroy/cleanup`).
//!
//! `serve` builds a `DockerRunner` and hands it to `oneai_orchestrator::run`,
//! which loads+reconciles the persisted routing table, starts the idle
//! hibernation sweep, and serves the axum control plane until Ctrl-C.
//! The client subcommands call the same HTTP API the web frontends use
//! (Bearer auth via `ONEAI_ORCHESTRATOR_SECRET`).

use std::sync::Arc;

use oneai_orchestrator::config::{OrchestratorConfig, ORCHESTRATOR_SECRET_ENV};
use oneai_orchestrator::docker::DockerRunner;

/// Default control-plane URL for client subcommands.
const DEFAULT_URL: &str = "http://127.0.0.1:9191";

// ─── serve (daemon) ─────────────────────────────────────────────────────────

pub fn cmd_orchestrator_serve(
    listen: Option<&str>,
    image: Option<&str>,
    registry: Option<&str>,
    idle_timeout: Option<u64>,
    provider_config: Option<&str>,
) {
    println!("🤖 OneAI Orchestrator — cloud session control plane (MVS2)");

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(e) = rt.block_on(async move {
        let mut config = OrchestratorConfig::load_default()?
            .with_overrides(listen, image, registry, idle_timeout);
        if let Some(p) = provider_config {
            config.provider_config = Some(std::path::PathBuf::from(p));
        }

        println!("   Listen:   {}", config.listen);
        println!("   Image:    {}", config.image);
        println!("   Registry: {}", config.registry_dir.display());
        println!(
            "   Idle hibernate: {}s",
            if config.idle_timeout_secs == 0 {
                "disabled".to_string()
            } else {
                config.idle_timeout_secs.to_string()
            }
        );
        if let Some(p) = &config.provider_config {
            println!("   Provider config (ro bind-mount): {}", p.display());
        }
        if std::env::var(ORCHESTRATOR_SECRET_ENV)
            .ok()
            .filter(|s| !s.is_empty())
            .is_none()
        {
            eprintln!(
                "Error: {ORCHESTRATOR_SECRET_ENV} must be set (frontend→orchestrator bearer secret)."
            );
            std::process::exit(1);
        }
        println!();

        // Health probe target: a container published on 0.0.0.0 is probed via
        // loopback (you can't "connect to" the wildcard literally, and under
        // colima the forwarded port lands on the host's loopback anyway).
        let probe_host = if config.container_bind_host == "0.0.0.0" {
            "127.0.0.1".to_string()
        } else {
            config.container_bind_host.clone()
        };
        let runner = Arc::new(DockerRunner::with_bin(&config.docker_bin, probe_host));
        let cancel = tokio_util::sync::CancellationToken::new();
        oneai_orchestrator::run(config, runner, cancel)
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })
    }) {
        eprintln!("Error running orchestrator: {e}");
        std::process::exit(1);
    }
}

// ─── client subcommands ─────────────────────────────────────────────────────

fn client_secret() -> String {
    match std::env::var(ORCHESTRATOR_SECRET_ENV)
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(s) => s,
        None => {
            eprintln!("Error: {ORCHESTRATOR_SECRET_ENV} must be set to talk to the orchestrator.");
            std::process::exit(1);
        }
    }
}

fn http_client() -> reqwest::Client {
    // Loopback control plane — never route through HTTP(S)_PROXY.
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("reqwest client")
}

fn url_or_default(url: Option<&str>) -> String {
    url.map(str::to_string)
        .unwrap_or_else(|| DEFAULT_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}

pub fn cmd_orchestrator_create(url: Option<&str>, id: Option<&str>) {
    let base = url_or_default(url);
    let secret = client_secret();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = rt.block_on(async {
        let mut body = serde_json::Map::new();
        if let Some(id) = id {
            body.insert("session_id".into(), serde_json::Value::String(id.into()));
        }
        let resp = http_client()
            .post(format!("{base}/v1/sessions"))
            .bearer_auth(&secret)
            .json(&serde_json::Value::Object(body))
            .send()
            .await?;
        let status = resp.status();
        let json: serde_json::Value = resp.json().await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((status, json))
    });
    match result {
        Ok((status, json)) if status.is_success() => {
            println!("Created session: {}", json["session"]["session_id"]);
            println!("  state:   {}", json["session"]["state"]);
            println!(
                "  ws_url:  {}{}",
                base.replace("http://", "ws://"),
                json["ws_url"]
            );
        }
        Ok((status, json)) => {
            eprintln!("Error {status}: {}", json["error"]);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

pub fn cmd_orchestrator_list(url: Option<&str>) {
    let base = url_or_default(url);
    let secret = client_secret();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = rt.block_on(async {
        let resp = http_client()
            .get(format!("{base}/v1/sessions"))
            .bearer_auth(&secret)
            .send()
            .await?;
        let json: serde_json::Value = resp.json().await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(json)
    });
    match result {
        Ok(json) => {
            let sessions = json["sessions"].as_array().cloned().unwrap_or_default();
            if sessions.is_empty() {
                println!("No orchestrated sessions.");
                return;
            }
            println!(
                "{:<36} {:<12} {:<8} {:<24} UPDATED",
                "SESSION", "STATE", "PORT", "CONTAINER"
            );
            for s in &sessions {
                println!(
                    "{:<36} {:<12} {:<8} {:<24} {}",
                    s["session_id"].as_str().unwrap_or("?"),
                    s["state"].as_str().unwrap_or("?"),
                    s["host_port"]
                        .as_u64()
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "-".into()),
                    s["container_name"].as_str().unwrap_or("-"),
                    s["updated_at"].as_str().unwrap_or("?"),
                );
                if let Some(err) = s["last_error"].as_str() {
                    println!("    └─ {err}");
                }
            }
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

pub fn cmd_orchestrator_status(url: Option<&str>, id: &str) {
    let base = url_or_default(url);
    let secret = client_secret();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = rt.block_on(async {
        let resp = http_client()
            .get(format!("{base}/v1/sessions/{id}"))
            .bearer_auth(&secret)
            .send()
            .await?;
        let status = resp.status();
        let json: serde_json::Value = resp.json().await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((status, json))
    });
    match result {
        Ok((status, json)) if status.is_success() => {
            println!("{}", serde_json::to_string_pretty(&json).unwrap());
        }
        Ok((status, json)) => {
            eprintln!("Error {status}: {}", json["error"]);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

pub fn cmd_orchestrator_destroy(url: Option<&str>, id: &str) {
    let base = url_or_default(url);
    let secret = client_secret();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = rt.block_on(async {
        let resp = http_client()
            .delete(format!("{base}/v1/sessions/{id}"))
            .bearer_auth(&secret)
            .send()
            .await?;
        let status = resp.status();
        let json: serde_json::Value = resp.json().await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((status, json))
    });
    match result {
        Ok((status, _)) if status.is_success() => {
            println!("Destroyed session: {id} (container + volumes removed)");
        }
        Ok((status, json)) => {
            eprintln!("Error {status}: {}", json["error"]);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

/// Remove leftover `oneai-orch-*` containers + volumes directly via the
/// docker CLI (acceptance teardown / crash cleanup — no orchestrator needed).
pub fn cmd_orchestrator_cleanup() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let ls = tokio::process::Command::new("docker")
            .args(["ps", "-aq", "--filter", "name=oneai-orch-"])
            .output()
            .await
            .expect("docker ps");
        let ids: Vec<String> = String::from_utf8_lossy(&ls.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if !ids.is_empty() {
            let mut cmd = tokio::process::Command::new("docker");
            cmd.arg("rm").arg("-f").args(&ids);
            let _ = cmd.status().await;
            println!("Removed {} container(s).", ids.len());
        } else {
            println!("No oneai-orch-* containers.");
        }
        let lv = tokio::process::Command::new("docker")
            .args(["volume", "ls", "-q", "--filter", "name=oneai-orch-"])
            .output()
            .await
            .expect("docker volume ls");
        let vols: Vec<String> = String::from_utf8_lossy(&lv.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if !vols.is_empty() {
            let mut cmd = tokio::process::Command::new("docker");
            cmd.arg("volume").arg("rm").arg("-f").args(&vols);
            let _ = cmd.status().await;
            println!("Removed {} volume(s).", vols.len());
        } else {
            println!("No oneai-orch-* volumes.");
        }
    });
}
