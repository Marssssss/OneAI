//! Studio command — launch the OneAI Studio Web UI.

use oneai_studio::{StudioConfig, serve};

/// Launch the Studio Web UI server.
///
/// Starts an axum HTTP server on the specified port with:
/// - REST API endpoints for sessions, graphs, checkpoints, domain packs, tools
/// - WebSocket real-time event streaming
/// - Embedded HTML/JS/CSS frontend for visualization
pub fn cmd_studio(port: u16, domain: Option<&str>) {
    println!("🤖 OneAI Studio — Playground/Studio Web UI");
    println!("   Port: {}", port);
    if let Some(d) = domain {
        println!("   Domain: {}", d);
    }
    println!();

    let config = StudioConfig::with_port(port);

    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
    if let Err(e) = rt.block_on(serve(config)) {
        eprintln!("Error starting Studio server: {}", e);
        std::process::exit(1);
    }
}
