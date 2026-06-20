//! Studio HTTP server — starts the axum server and binds to a port.

use std::sync::Arc;
use std::net::SocketAddr;

use tower_http::cors::{CorsLayer, Any};

use crate::state::StudioState;
use crate::routes;

// ─── Server Configuration ────────────────────────────────────────────

/// Configuration for the Studio server.
#[derive(Debug, Clone)]
pub struct StudioConfig {
    /// The address to bind the HTTP server to.
    pub addr: SocketAddr,

    /// Whether to enable CORS (for local development).
    pub cors_enabled: bool,
}

impl Default for StudioConfig {
    fn default() -> Self {
        Self {
            addr: SocketAddr::from(([0, 0, 0, 0], 3000)),
            cors_enabled: true,
        }
    }
}

impl StudioConfig {
    /// Create a config with a custom port.
    pub fn with_port(port: u16) -> Self {
        Self {
            addr: SocketAddr::from(([0, 0, 0, 0], port)),
            cors_enabled: true,
        }
    }
}

// ─── Start Server ────────────────────────────────────────────────────

/// Start the Studio HTTP server.
///
/// Creates the axum Router, applies CORS middleware (if enabled),
/// and binds to the configured address. Returns when the server
/// shuts down (via Ctrl+C or `tokio::signal`).
pub async fn serve(config: StudioConfig) -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(StudioState::new_default());
    serve_with_state(config, state).await
}

/// Start the Studio server with an existing StudioState.
///
/// Use this when you have an AgentLoop running and want to
/// attach the Studio to observe its execution in real-time.
pub async fn serve_with_state(
    config: StudioConfig,
    state: Arc<StudioState>,
) -> Result<(), Box<dyn std::error::Error>> {
    let router = routes::build_router(state);

    // Add CORS layer for local development
    let app = if config.cors_enabled {
        router.layer(CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any))
    } else {
        router
    };

    // Bind and serve
    let listener = tokio::net::TcpListener::bind(config.addr).await?;
    tracing::info!("OneAI Studio server listening on http://{}", config.addr);

    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_studio_config_default() {
        let config = StudioConfig::default();
        assert_eq!(config.addr.port(), 3000);
        assert!(config.cors_enabled);
    }

    #[test]
    fn test_studio_config_custom_port() {
        let config = StudioConfig::with_port(8080);
        assert_eq!(config.addr.port(), 8080);
    }
}
