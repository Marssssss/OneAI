//! MCP server management commands.
//!
//! Subcommands for managing MCP server plugins and running as an MCP server:
//!   oneai mcp serve    — Run OneAI as an MCP server (Stdio mode)
//!   oneai mcp list     — List configured MCP servers
//!   oneai mcp add      — Add an MCP server config
//!   oneai mcp remove   — Remove an MCP server config
//!   oneai mcp connect  — Test connecting to an MCP server
//!   oneai mcp oauth    — OAuth 2.0 login / refresh / status / logout for an
//!                        HTTP-transport MCP server (issue #31 Stage 3).

use std::sync::Arc;

use oneai_core::{PermissionLevel as Level, ToolExposure};
use oneai_tool::{CalculatorTool, ToolRegistry};

use oneai_mcp::{
    McpOAuthConfig, McpPluginEntry, McpPluginRegistry, McpPluginSource, McpServerHost,
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
        registry
            .register(Arc::new(CalculatorTool::new()))
            .await
            .unwrap();

        // If a domain pack is specified, register its tools
        if let Some(domain_name) = domain {
            if let Some(pack) = super::cmd_pack::get_builtin_pack(domain_name, ".") {
                for tool in &pack.tools {
                    registry.register(tool.clone()).await.unwrap();
                }
                tracing::info!(
                    "Domain pack '{}' loaded — {} tools registered",
                    domain_name,
                    pack.tools.len()
                );
            } else {
                eprintln!(
                    "Warning: Domain pack '{}' not found, using default tools",
                    domain_name
                );
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
            _ => "unknown".to_string(),
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
pub fn cmd_mcp_add(
    name: &str,
    transport: &str,
    command: Option<&str>,
    url: Option<&str>,
    args: Option<&str>,
    enabled: bool,
    lazy: bool,
) {
    let mut registry = McpPluginRegistry::from_config_file();

    // Build the source based on transport type
    let source = match transport {
        "stdio" => {
            let cmd = command.unwrap_or_else(|| {
                eprintln!("Error: --command required for stdio transport");
                std::process::exit(1);
            });
            let args_list = args
                .map(|a| a.split(',').map(|s| s.trim().to_string()).collect())
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
            eprintln!(
                "Error: Unknown transport type '{}'. Use: stdio, sse, streamable_http",
                transport
            );
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
        lazy,
        ..Default::default()
    };

    registry.add_entry(entry);

    if let Err(e) = registry.save_config() {
        eprintln!("Error saving config: {}", e);
        return;
    }

    println!(
        "✅ MCP server '{}' added (transport: {}, enabled: {}, lazy: {})",
        name, transport, enabled, lazy
    );
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
        let registry = McpPluginRegistry::from_config_file();

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

/// Probe one (or all) server(s): connect, list the namespaced tool names the
/// model would see, then disconnect **only that server** (exercising the
/// per-server `disconnect_server` path). Differs from `connect` in that it
/// disconnects the named server alone rather than `disconnect_all`, and
/// reports the `mcp__<server>__<tool>` identifiers registered into the
/// `ToolRegistry`.
pub fn cmd_mcp_status(name: Option<&str>) {
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");

    rt.block_on(async {
        let registry = McpPluginRegistry::from_config_file();

        // Targets: the named server, or every configured entry.
        let targets: Vec<String> = match name {
            Some(n) => {
                if registry.get_entry(n).is_none() {
                    eprintln!("MCP server '{}' not found in config.", n);
                    return;
                }
                vec![n.to_string()]
            }
            None => registry
                .list_entries()
                .iter()
                .map(|e| e.name.clone())
                .collect(),
        };

        println!("🔌 MCP server status probe\n");

        for srv in &targets {
            println!("── {} ──", srv);
            match registry.connect_server(srv).await {
                Ok(tool_names) => {
                    println!("  ✅ live — {} tool(s)", tool_names.len());
                    for t in &tool_names {
                        println!("    • {}", t);
                    }
                    if tool_names.is_empty() {
                        println!("    (no tools advertised)");
                    }
                }
                Err(e) => {
                    println!("  ❌ unreachable — {}", e);
                }
            }
            // Per-server disconnect (not disconnect_all).
            let _ = registry.disconnect_server(srv).await;
            println!();
        }
    });
}

/// Parse a CLI string into a `PermissionLevel` (read/standard/full) or
/// `ToolExposure` (direct/deferred/...) via the same serde rename rules the
/// TOML config uses.
fn parse_level(s: &str) -> std::result::Result<Level, String> {
    serde_json::from_str::<Level>(&format!("\"{}\"", s))
        .map_err(|_| format!("unknown permission level '{}': use read/standard/full", s))
}
fn parse_exposure(s: &str) -> std::result::Result<ToolExposure, String> {
    serde_json::from_str::<ToolExposure>(&format!("\"{}\"", s)).map_err(|_| {
        format!(
            "unknown exposure '{}': use direct/deferred/deferred_model_only/direct_model_only/code_mode_only/hidden",
            s
        )
    })
}

/// Inspect or set per-server tool permission + exposure policy.
///
/// `oneai mcp perm <server> --list`                — print the current policy.
/// `oneai mcp perm <server> --level read`          — set the server default level.
/// `oneai mcp perm <server> --tool x --exposure hidden` — set a per-tool override.
pub fn cmd_mcp_perm(
    server: &str,
    tool: Option<&str>,
    level: Option<&str>,
    exposure: Option<&str>,
    list: bool,
) {
    let mut registry = McpPluginRegistry::from_config_file();

    let mut entry = match registry.get_entry(server) {
        Some(e) => e.clone(),
        None => {
            eprintln!("MCP server '{}' not found in config.", server);
            return;
        }
    };

    if !list && level.is_none() && exposure.is_none() {
        eprintln!("Nothing to do — pass --list, --level, or --exposure.");
        return;
    }

    // Apply mutations.
    if let Some(lvl) = level {
        match parse_level(lvl) {
            Ok(l) => {
                if let Some(t) = tool {
                    entry.permissions.tool_overrides.insert(t.to_string(), l);
                } else {
                    entry.permissions.default_level = l;
                }
            }
            Err(e) => {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        }
    }
    if let Some(exp) = exposure {
        match parse_exposure(exp) {
            Ok(e) => {
                if let Some(t) = tool {
                    entry.permissions.tool_exposure.insert(t.to_string(), e);
                } else {
                    eprintln!("--exposure requires --tool (server-wide exposure default is always Direct).");
                    std::process::exit(1);
                }
            }
            Err(e) => {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        }
    }

    // Persist (only if something changed).
    if !list {
        registry.add_entry(entry.clone());
        if let Err(e) = registry.save_config() {
            eprintln!("Error saving config: {}", e);
            return;
        }
    }

    // Print the (possibly updated) policy.
    println!("🔌 MCP server '{}' permission policy", server);
    println!(
        "  default_level: {}",
        fmt_level(entry.permissions.default_level)
    );
    if entry.permissions.tool_overrides.is_empty() {
        println!("  tool_overrides: (none)");
    } else {
        println!("  tool_overrides:");
        for (t, l) in &entry.permissions.tool_overrides {
            println!("    {} → {}", t, fmt_level(*l));
        }
    }
    if entry.permissions.tool_exposure.is_empty() {
        println!("  tool_exposure: (none — all Direct)");
    } else {
        println!("  tool_exposure:");
        for (t, e) in &entry.permissions.tool_exposure {
            println!("    {} → {}", t, fmt_exposure(*e));
        }
    }
    if !list {
        println!("\nSaved to ~/.oneai/mcp_servers.toml");
        println!("Reconnect or restart the app for the policy to take effect on live wrappers.");
    }
}

fn fmt_level(l: Level) -> &'static str {
    match l {
        Level::Read => "read",
        Level::Standard => "standard",
        Level::Full => "full",
    }
}

fn fmt_exposure(e: ToolExposure) -> &'static str {
    match e {
        ToolExposure::Direct => "direct",
        ToolExposure::Deferred => "deferred",
        ToolExposure::DeferredModelOnly => "deferred_model_only",
        ToolExposure::DirectModelOnly => "direct_model_only",
        ToolExposure::CodeModeOnly => "code_mode_only",
        ToolExposure::Hidden => "hidden",
        _ => "unknown",
    }
}

// ─── OAuth ───────────────────────────────────────────────────────────────────

/// Run the OAuth login flow for a server (issue #31 Stage 3).
pub fn cmd_mcp_oauth_login(server: &str, manual: bool, port: Option<u16>) {
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
    rt.block_on(async {
        // Apply a --port override to the entry's oauth config before login
        // (avoids requiring a separate `set` step just to pin a port).
        let mut registry = McpPluginRegistry::from_config_file();
        if let Some(p) = port {
            let mut entry = match registry.get_entry(server) {
                Some(e) => e.clone(),
                None => {
                    eprintln!("MCP server '{}' not found in config.", server);
                    return;
                }
            };
            let oauth = entry.oauth.get_or_insert_with(McpOAuthConfig::default);
            oauth.redirect_port = Some(p);
            registry.add_entry(entry);
            let _ = registry.save_config();
        }
        match registry.oauth_login(server, manual).await {
            Ok(tokens) => {
                println!(
                    "✅ OAuth login complete for '{}' — token_type={}, expires_at={:?}",
                    server, tokens.token_type, tokens.expires_at
                );
            }
            Err(e) => {
                eprintln!("❌ OAuth login failed: {}", e);
                eprintln!("  Make sure the server entry has an [oauth] table and is an");
                eprintln!("  SSE or streamable_http transport. Use `oneai mcp oauth <server> set` to configure.");
            }
        }
    });
}

/// Force a refresh of a server's stored OAuth tokens.
pub fn cmd_mcp_oauth_refresh(server: &str) {
    let rt = tokio::runtime::Runtime::new().expect("Tokio runtime creation");
    rt.block_on(async {
        let registry = McpPluginRegistry::from_config_file();
        match registry.oauth_refresh(server).await {
            Ok(tokens) => {
                println!(
                    "✅ OAuth tokens refreshed for '{}' — expires_at={:?}",
                    server, tokens.expires_at
                );
            }
            Err(e) => {
                eprintln!("❌ OAuth refresh failed: {}", e);
            }
        }
    });
}

/// Show stored OAuth token metadata for a server (redacted access token).
pub fn cmd_mcp_oauth_status(server: &str) {
    let registry = McpPluginRegistry::from_config_file();
    let entry = match registry.get_entry(server) {
        Some(e) => e,
        None => {
            eprintln!("MCP server '{}' not found in config.", server);
            return;
        }
    };
    println!("🔌 MCP OAuth status — '{}'\n", server);
    match entry.oauth.as_ref() {
        Some(cfg) => {
            println!("  Configured OAuth: yes");
            println!(
                "    resource_url: {}",
                cfg.resource_url
                    .as_deref()
                    .unwrap_or("(from transport url)")
            );
            println!("    scopes: {:?}", cfg.scopes);
            println!(
                "    client_id: {}",
                cfg.client_id.as_deref().unwrap_or("(dynamic registration)")
            );
            println!(
                "    dynamic_registration: {} | pkce: {}",
                cfg.use_dynamic_registration, cfg.pkce
            );
        }
        None => {
            println!(
                "  Configured OAuth: no (run `oneai mcp oauth {} set ...`)",
                server
            );
        }
    }

    match registry.oauth_status(server) {
        Some(tokens) => {
            let preview = redact(&tokens.access_token);
            println!("\n  Stored tokens:");
            println!("    access_token: {}", preview);
            println!("    token_type:   {}", tokens.token_type);
            println!(
                "    refresh_token: {}",
                if tokens.refresh_token.is_some() {
                    "present"
                } else {
                    "absent"
                }
            );
            println!("    expires_at:    {:?}", tokens.expires_at);
            println!("    expired:       {}", tokens.is_expired());
        }
        None => {
            println!(
                "\n  Stored tokens: none (run `oneai mcp oauth {} login`)",
                server
            );
        }
    }
}

/// Delete a server's stored OAuth tokens (logout).
pub fn cmd_mcp_oauth_logout(server: &str) {
    let registry = McpPluginRegistry::from_config_file();
    match registry.oauth_logout(server) {
        Ok(()) => println!("✅ OAuth tokens deleted for '{}'.", server),
        Err(e) => eprintln!("❌ OAuth logout failed: {}", e),
    }
}

/// Configure the `[servers.<name>.oauth]` table from CLI flags.
#[allow(clippy::too_many_arguments)]
pub fn cmd_mcp_oauth_set(
    server: &str,
    client_id: Option<&str>,
    client_secret: Option<&str>,
    scopes: Option<&str>,
    no_dynamic_registration: bool,
    no_pkce: bool,
    resource_url: Option<&str>,
    redirect_port: Option<u16>,
) {
    let mut registry = McpPluginRegistry::from_config_file();
    let mut entry = match registry.get_entry(server) {
        Some(e) => e.clone(),
        None => {
            eprintln!("MCP server '{}' not found in config.", server);
            return;
        }
    };
    // Build the oauth config — `Set` replaces the whole oauth table for
    // clarity; callers wanting partial edits can edit the TOML directly.
    let oauth = McpOAuthConfig {
        resource_url: resource_url.map(str::to_string),
        scopes: scopes
            .map(|s| {
                s.split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        client_id: client_id.map(str::to_string),
        client_secret: client_secret.map(str::to_string),
        redirect_port,
        use_dynamic_registration: !no_dynamic_registration,
        pkce: !no_pkce,
    };
    entry.oauth = Some(oauth);
    registry.add_entry(entry);
    if let Err(e) = registry.save_config() {
        eprintln!("Error saving config: {}", e);
        return;
    }
    println!(
        "✅ OAuth config saved for '{}' → ~/.oneai/mcp_servers.toml",
        server
    );
    println!("   Run `oneai mcp oauth {} login` to authorize.", server);
}

fn redact(token: &str) -> String {
    let len = token.len();
    if len <= 8 {
        return "*".repeat(len);
    }
    format!("{}…{}", &token[..4], &token[len - 4..])
}
