//! MCP server management commands.
//!
//! Subcommands for managing MCP server plugins and running as an MCP server:
//!   oneai mcp serve   — Run OneAI as an MCP server (Stdio mode)
//!   oneai mcp list    — List configured MCP servers
//!   oneai mcp add     — Add an MCP server config
//!   oneai mcp remove  — Remove an MCP server config
//!   oneai mcp connect — Test connecting to an MCP server

use std::sync::Arc;

use oneai_tool::ToolRegistry;
use oneai_tool::CalculatorTool;

use oneai_mcp::{
    McpServerHost, McpPluginRegistry,
    McpPluginEntry, McpPluginSource,
};

/// Run OneAI as an MCP server via Stdio transport.
///
/// This starts the MCP protocol handler that reads from stdin and
/// writes to stdout using Content-Length framing. External MCP clients
/// (Claude Code, Cursor, VS Code, etc.) can launch `oneai mcp serve`
/// as a subprocess and interact with it via the MCP JSON-RPC protocol.
///
/// The server exposes all registered OneAI tools as MCP tool definitions.
/// By default, it includes the calculator tool and any domain pack tools.
pub fn cmd_mcp_serve(domain: Option<&str>) {
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");

    rt.block_on(async {
        let registry = Arc::new(ToolRegistry::new());

        // Register basic tools
        registry.register(Arc::new(CalculatorTool::new())).await.unwrap();

        // If a domain pack is specified, register its tools
        if let Some(domain_name) = domain {
            if let Some(pack) = super::cmd_pack::get_builtin_pack(domain_name, ".") {
                for tool in &pack.tools {
                    registry.register(tool.clone()).await.unwrap();
                }
                tracing::info!("Domain pack '{}' loaded — {} tools registered", domain_name, pack.tools.len());
            } else {
                eprintln!("Warning: Domain pack '{}' not found, using default tools", domain_name);
            }
        }

        // Create and run the MCP server host
        let host = McpServerHost::new(registry);

        tracing::info!("Starting MCP server (Stdio mode) — serving OneAI tools");

        match host.run_stdio().await {
            Ok(_) => {
                tracing::info!("MCP server shutdown gracefully");
            }
            Err(e) => {
                eprintln!("MCP server error: {}", e);
            }
        }
    });
}

/// List all configured MCP servers.
///
/// Shows both builtin defaults and user-configured servers from
/// `~/.oneai/mcp_servers.toml`, with their transport type and status.
pub fn cmd_mcp_list() {
    let registry = McpPluginRegistry::from_config_file();

    println!("🔌 MCP Server Plugins\n");

    let entries = registry.list_entries();
    if entries.is_empty() {
        println!("  No MCP servers configured.");
        println!("  Use 'oneai mcp add' to add a server, or edit ~/.oneai/mcp_servers.toml");
        return;
    }

    for entry in entries {
        let status_icon = if entry.enabled { "✅" } else { "❌" };
        let transport_type = match &entry.source {
            McpPluginSource::Stdio { command, .. } => {
                format!("stdio: {}", command)
            }
            McpPluginSource::Sse { url, .. } => {
                format!("sse: {}", url)
            }
            McpPluginSource::StreamableHttp { url, .. } => {
                format!("streamable_http: {}", url)
            }
            _ => {
                format!("unknown")
            }
        };

        println!("  {} {} — {}", status_icon, entry.name, entry.description);
        println!("     Transport: {}", transport_type);

        if entry.requires_api_key {
            let key_env = entry.api_key_env.as_deref().unwrap_or("unknown");
            let has_key = std::env::var(key_env).is_ok();
            let key_icon = if has_key { "🔑 set" } else { "🔑 missing" };
            println!("     API key: {} ({})", key_env, key_icon);
        }

        if !entry.tags.is_empty() {
            println!("     Tags: {}", entry.tags.join(", "));
        }

        println!();
    }

    println!("Use 'oneai mcp connect <name>' to test a connection");
}

/// Add an MCP server configuration.
///
/// Creates a new entry in `~/.oneai/mcp_servers.toml`.
pub fn cmd_mcp_add(name: &str, transport: &str, command: Option<&str>, url: Option<&str>, args: Option<&str>, enabled: bool) {
    let mut registry = McpPluginRegistry::from_config_file();

    // Build the source based on transport type
    let source = match transport {
        "stdio" => {
            let cmd = command.unwrap_or_else(|| {
                eprintln!("Error: --command required for stdio transport");
                std::process::exit(1);
            });
            let args_list = args.map(|a| a.split(',').map(|s| s.trim().to_string()).collect())
                .unwrap_or_default();
            McpPluginSource::Stdio {
                command: cmd.to_string(),
                args: args_list,
                env: std::collections::HashMap::new(),
            }
        }
        "sse" => {
            let url_val = url.unwrap_or_else(|| {
                eprintln!("Error: --url required for SSE transport");
                std::process::exit(1);
            });
            McpPluginSource::Sse {
                url: url_val.to_string(),
                headers: std::collections::HashMap::new(),
            }
        }
        "streamable_http" => {
            let url_val = url.unwrap_or_else(|| {
                eprintln!("Error: --url required for streamable_http transport");
                std::process::exit(1);
            });
            McpPluginSource::StreamableHttp {
                url: url_val.to_string(),
                headers: std::collections::HashMap::new(),
            }
        }
        _ => {
            eprintln!("Error: Unknown transport type '{}'. Use: stdio, sse, streamable_http", transport);
            std::process::exit(1);
        }
    };

    let entry = McpPluginEntry {
        name: name.to_string(),
        description: format!("MCP server: {}", name),
        source,
        enabled,
        requires_api_key: false,
        api_key_env: None,
        tags: vec![name.to_string()],
    };

    registry.add_entry(entry);

    if let Err(e) = registry.save_config() {
        eprintln!("Error saving config: {}", e);
        return;
    }

    println!("✅ MCP server '{}' added (transport: {}, enabled: {})", name, transport, enabled);
    println!("   Config saved to: ~/.oneai/mcp_servers.toml");
    println!("   Use 'oneai mcp connect {}' to test the connection", name);
}

/// Remove an MCP server configuration.
pub fn cmd_mcp_remove(name: &str) {
    let mut registry = McpPluginRegistry::from_config_file();

    if registry.get_entry(name).is_none() {
        eprintln!("MCP server '{}' not found in config.", name);
        return;
    }

    let removed = registry.remove_entry(name);
    if let Some(entry) = removed {
        if let Err(e) = registry.save_config() {
            eprintln!("Error saving config: {}", e);
            return;
        }

        println!("✅ MCP server '{}' removed", entry.name);
        println!("   Config saved to: ~/.oneai/mcp_servers.toml");
    }
}

/// Test connecting to an MCP server and show discovered tools.
pub fn cmd_mcp_connect(name: &str) {
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");

    rt.block_on(async {
        let mut registry = McpPluginRegistry::from_config_file();

        let entry = registry.get_entry(name);
        if entry.is_none() {
            eprintln!("MCP server '{}' not found in config.", name);
            return;
        }

        let entry = entry.unwrap();
        println!("🔌 Connecting to MCP server '{}'...\n", name);

        // Temporarily enable if disabled
        if !entry.enabled {
            println!("  Note: server is disabled in config, attempting connection anyway");
        }

        match registry.connect_server(name).await {
            Ok(tool_names) => {
                println!("  ✅ Connected successfully!");
                println!("  Discovered {} tools:", tool_names.len());
                for tool_name in &tool_names {
                    println!("    • {}", tool_name);
                }
                if tool_names.is_empty() {
                    println!("    (no tools available on this server)");
                }
            }
            Err(e) => {
                println!("  ❌ Connection failed: {}", e);
                println!("  Possible causes:");
                println!("    - Server command not found (for stdio transport)");
                println!("    - Server URL unreachable (for SSE/HTTP transport)");
                println!("    - API key not set (if requires_api_key = true)");
                println!("    - MCP protocol version mismatch");
            }
        }

        // Cleanup — disconnect
        let _ = registry.disconnect_all().await;
    });
}
