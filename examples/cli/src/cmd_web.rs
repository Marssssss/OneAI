//! `oneai web` — one-command webUI launch (对标 deepseek-harness `npx @deepseek-ai/dsh web`).
//!
//! Builds the engine (same `build_engine_server` the app-server uses), then
//! serves the prebuilt SPA static assets + the `/ws` JSON-RPC endpoint on a
//! single port via `oneai_app_server::serve_web`, and opens the default
//! browser. No separate Vite dev server / app-server process for end users —
//! `npx oneai-cli web` (the npm package bundles the dist + fetches the
//! platform binary on install) or `oneai web` (global install).
//!
//! The web dist is platform-independent JS; it ships inside the npm tarball
//! (built at `npm publish` via `prepublishOnly`) and the launcher sets
//! `ONEAI_WEB_DIST` to the bundled path. For cargo/binary users, `--dist` or
//! auto-detect (`./platforms/web/dist` after `npm run build`) locates it.

use std::path::PathBuf;

use oneai_app_server::serve_web;

use crate::cmd_app_server::{build_engine_server, init_stderr_logging};
use crate::config::OneaiConfig;

/// Resolve the web dist dir. Order: `--dist` > `ONEAI_WEB_DIST` env >
/// `<exe_dir>/web-dist` (binary-bundled) > `./platforms/web/dist` (dev) >
/// `~/.oneai/web-dist`. A candidate counts only if it has `index.html`.
fn resolve_web_dist(dist_arg: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = dist_arg {
        return Some(PathBuf::from(p));
    }
    if let Ok(p) = std::env::var("ONEAI_WEB_DIST") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let cand = parent.join("web-dist");
            if cand.join("index.html").exists() {
                return Some(cand);
            }
        }
    }
    let cand = PathBuf::from("platforms/web/dist");
    if cand.join("index.html").exists() {
        return Some(cand);
    }
    if let Some(home) = dirs::home_dir() {
        let cand = home.join(".oneai").join("web-dist");
        if cand.join("index.html").exists() {
            return Some(cand);
        }
    }
    None
}

/// Open `url` in the platform default browser. Best-effort — a failure just
/// logs to stderr (the URL is already printed).
fn open_browser(url: &str) {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/c", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    match std::process::Command::new(program).args(&args).spawn() {
        Ok(_) => {}
        Err(e) => eprintln!("   (couldn't open browser: {e}; open {url} manually)"),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_web(
    config: &OneaiConfig,
    port: u16,
    host: String,
    dist: Option<&str>,
    no_open: bool,
    domain: Option<&str>,
    model: Option<&str>,
    user: Option<&str>,
) {
    init_stderr_logging();

    eprintln!("🌐 OneAI web — SPA + JSON-RPC /ws on one port");
    eprintln!("   listen: http://{host}:{port}");

    // Resolve the dist BEFORE building the engine: a missing dist is a clear,
    // actionable error (build it / use the npm package), not a silent ws-only
    // server that 404s on `/`.
    let dist_dir = match resolve_web_dist(dist) {
        Some(d) => {
            eprintln!("   web dist: {}", d.display());
            d
        }
        None => {
            eprintln!(
                "Error: web dist not found. Build it (run from the repo):\n  \
                 cd platforms/web && npm install && npm run build\n  \
                 or set --dist <path> / ONEAI_WEB_DIST=<path>, or use `npx oneai-cli web` \
                 (the npm package bundles the dist)."
            );
            std::process::exit(2);
        }
    };

    let provider_config = config.to_model_config_with_overrides(model);
    if provider_config.is_none() {
        eprintln!("⚠️  No LLM provider configured (set ONEAI_API_KEY / ONEAI_BASE_URL).");
        eprintln!("   The web server will start, but turns will reject.\n");
    }

    let addr: std::net::SocketAddr = match format!("{host}:{port}").parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Error: invalid host/port: {e}");
            std::process::exit(2);
        }
    };

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(e) = rt.block_on(async move {
        let es = build_engine_server(config, provider_config, domain, user).await?;

        let (handle, bound) = serve_web(
            addr,
            Some(dist_dir),
            es.bus,
            es.scenario_store,
            es.conversation_store,
            es.feedback_store,
            es.probe,
        )
        .await?;

        let url = format!("http://{bound}");
        eprintln!("✅ webUI ready: {url}");
        eprintln!("   Ctrl-C to stop.");
        if !no_open {
            open_browser(&url);
        }

        tokio::select! {
            _ = handle => eprintln!("\n web: server exited."),
            _ = tokio::signal::ctrl_c() => eprintln!("\n Interrupted — shutting down."),
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }) {
        eprintln!("Error running web: {e}");
        std::process::exit(1);
    }
}
