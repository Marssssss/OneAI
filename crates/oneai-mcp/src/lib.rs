//! # OneAI MCP
//!
//! MCP Server hosting, Client, Plugin Registry, and Discovery for the
//! Model Context Protocol ecosystem.
//!
//! This crate provides three major capabilities:
//!
//! 1. **MCP Server Host** — Expose OneAI's tools as an MCP server, enabling
//!    external MCP clients (Claude Code, Cursor, VS Code, etc.) to discover
//!    and invoke OneAI tools via the MCP JSON-RPC protocol.
//!
//! 2. **MCP Client** — Connect to external MCP servers as a client, discover
//!    their tools, and invoke them. Wraps the existing `McpServerManager`
//!    infrastructure for a simpler, standalone API.
//!
//! 3. **MCP Plugin Registry** — Config-based management of MCP server plugins,
//!    mirroring the DomainPack market pattern.
//!
//! 4. **MCP Discovery** — One-shot "connect_and_discover" for quick inspection
//!    of what tools an MCP server offers.
//!
//! ## MCP Server Host
//!
//! The `McpServerHost` serves OneAI's `ToolRegistry` tools via the MCP protocol:
//! - `initialize` → handshake with client capabilities
//! - `tools/list` → list all registered OneAI tools as MCP tool definitions
//! - `tools/call` → invoke a OneAI tool and return MCP-format content blocks
//!
//! ## MCP Client
//!
//! The `McpClient` provides a standalone API for connecting to external MCP servers:
//! ```ignore
//! let client = McpClient::stdio("npx", &["@anthropic/mcp-server-filesystem"]);
//! client.connect().await?;
//! let tools = client.discover_tools().await?;
//! client.disconnect().await?;
//! ```
//!
//! ## MCP Discovery
//!
//! One-shot discovery for quick tool inspection:
//! ```ignore
//! let tools = McpDiscovery::discover_stdio("npx", &["@anthropic/mcp-server-filesystem"]).await?;
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
pub mod client;
pub mod discovery;
pub mod error;

pub use server::*;
pub use transport::*;
pub use handler::*;
pub use router::*;
pub use plugin::*;
pub use config::*;
pub use client::McpClient;
pub use discovery::McpDiscovery;
pub use error::{McpError, Result as McpResult};
