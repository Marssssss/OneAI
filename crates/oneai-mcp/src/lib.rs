//! # OneAI MCP
//!
//! MCP Server hosting and Plugin Registry for the Model Context Protocol ecosystem.
//!
//! This crate provides two major capabilities:
//!
//! 1. **MCP Server Host** — Expose OneAI's tools as an MCP server, enabling
//!    external MCP clients (Claude Code, Cursor, VS Code, etc.) to discover
//!    and invoke OneAI tools via the MCP JSON-RPC protocol.
//!
//! 2. **MCP Plugin Registry** — Config-based management of MCP server plugins,
//!    mirroring the DomainPack market pattern. Enables discovery, installation,
//!    connection, and health monitoring of external MCP servers.
//!
//! ## MCP Server Host
//!
//! The `McpServerHost` serves OneAI's `ToolRegistry` tools via the MCP protocol:
//! - `initialize` → handshake with client capabilities
//! - `tools/list` → list all registered OneAI tools as MCP tool definitions
//! - `tools/call` → invoke a OneAI tool and return MCP-format content blocks
//! - `resources/list` → list MCP resources (basic implementation)
//!
//! Transport modes:
//! - **Stdio** — reads from stdin, writes to stdout with Content-Length framing
//! - **SSE** — HTTP endpoint for remote clients (future)
//! - **StreamableHttp** — HTTP with session management (future)
//!
//! ## MCP Plugin Registry
//!
//! The `McpPluginRegistry` manages external MCP server configurations:
//! - Load from `~/.oneai/mcp_servers.toml` config file
//! - Connect/disconnect servers, discover their tools
//! - Auto-register discovered tools into OneAI's ToolRegistry
//! - Health monitoring and lifecycle management
//!
//! ## Usage
//!
//! ```ignore
//! // Run as MCP server (Stdio mode):
//! let host = McpServerHost::new(tool_registry);
//! host.run_stdio().await?;
//!
//! // Load MCP plugins from config:
//! let registry = McpPluginRegistry::from_config_file();
//! let tools = registry.connect_all_and_discover().await?;
//!
//! // Add to AppBuilder:
//! let app = AppBuilder::new()
//!     .provider(provider)
//!     .mcp_servers_from_config()  // ← auto-connect MCP plugins
//!     .mcp_server_host()           // ← enable serving tools via MCP
//!     .build()?;
//! ```

//! # Stability
//!
//! This crate follows the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).
//! All public enums are annotated `#[non_exhaustive]` where appropriate to prevent
//! downstream breakage when new variants are added. Structs use constructor methods
//! for creation — direct struct literal construction is supported within this crate
//! but may be restricted in future versions via `#[non_exhaustive]`.
//!
//! Breaking changes will be signaled by a minor version bump (0.x → 0.y).
//! Patch versions (0.x.y → 0.x.z) are always backward-compatible.

pub mod server;
pub mod transport;
pub mod handler;
pub mod router;
pub mod plugin;
pub mod config;

pub use server::*;
pub use transport::*;
pub use handler::*;
pub use router::*;
pub use plugin::*;
pub use config::*;
