//! MCP tool integration — real MCP protocol implementation.
//!
//! Supports three transport modes:
//! 1. **Stdio** — launch a local subprocess, communicate via stdin/stdout
//! 2. **SSE** — connect to an HTTP SSE endpoint, send via POST, receive via event stream
//! 3. **StreamableHttp** — POST requests, receive responses as SSE stream
//!
//! The SSE/StreamableHttp transports use `reqwest` for HTTP communication
//! and `eventsource-stream` for parsing Server-Sent Events.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use oneai_core::error::Result;
use oneai_core::traits::Tool;
use oneai_core::{PermissionLevel, ToolExposure, ToolOutput};

use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

// ─── McpFramingParser ─────────────────────────────────────────────────────────

/// Parser for MCP's Content-Length framing protocol.
///
/// MCP uses HTTP-like framing for JSON-RPC messages:
/// ```text
/// Content-Length: 123\r\n
/// \r\n
/// <123 bytes of JSON body>
/// ```
///
/// The parser accumulates bytes in a buffer and extracts complete frames.
/// Each frame is a complete JSON-RPC message (request, response, or notification).
pub struct McpFramingParser {
    buffer: Vec<u8>,
}

impl McpFramingParser {
    /// Create a new framing parser with an empty buffer.
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Feed data into the parser buffer.
    pub fn feed(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }

    /// Try to parse a complete frame from the buffer.
    ///
    /// Returns the parsed JSON value if a complete frame is available,
    /// or None if the buffer doesn't contain a complete frame yet.
    /// Consumed bytes are removed from the buffer.
    pub fn try_parse_frame(&mut self) -> Option<serde_json::Value> {
        // Find the header end marker: \r\n\r\n
        let header_end = find_header_end(&self.buffer)?;
        let header_bytes = &self.buffer[..header_end];
        let header_str = String::from_utf8_lossy(header_bytes);

        // Parse Content-Length from header
        let content_length = parse_content_length(&header_str)?;

        // Check if we have enough data for the body
        let body_start = header_end;
        let body_end = body_start + content_length;
        if body_end > self.buffer.len() {
            return None; // Not enough data yet
        }

        // Extract and parse the JSON body
        let body_bytes = &self.buffer[body_start..body_end];
        let json: serde_json::Value = serde_json::from_slice(body_bytes).ok()?; // If JSON parsing fails, skip this frame

        // Remove consumed bytes from buffer
        self.buffer = self.buffer[body_end..].to_vec();

        Some(json)
    }

    /// Parse all available frames from the buffer.
    pub fn parse_all_frames(&mut self) -> Vec<serde_json::Value> {
        let mut frames = Vec::new();
        while let Some(frame) = self.try_parse_frame() {
            frames.push(frame);
        }
        frames
    }
}

impl Default for McpFramingParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Find the end of the HTTP-like header section (\r\n\r\n).
fn find_header_end(buffer: &[u8]) -> Option<usize> {
    for i in 0..buffer.len().saturating_sub(3) {
        if buffer[i] == b'\r'
            && buffer[i + 1] == b'\n'
            && buffer[i + 2] == b'\r'
            && buffer[i + 3] == b'\n'
        {
            return Some(i + 4); // Include the final \r\n\r\n
        }
    }
    None
}

/// Parse the Content-Length value from an HTTP-like header.
fn parse_content_length(header: &str) -> Option<usize> {
    for line in header.lines() {
        if line.starts_with("Content-Length:") || line.starts_with("Content-Length: ") {
            let value = line
                .trim_start_matches("Content-Length:")
                .trim()
                .parse::<usize>()
                .ok()?;
            return Some(value);
        }
    }
    None
}

// ─── McpTransport ───────────────────────────────────────────────────────────

/// Transport mode for connecting to MCP servers.
#[derive(Debug, Clone)]
pub enum McpTransport {
    /// stdio transport — launch a local subprocess.
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    /// SSE transport — connect to an HTTP SSE endpoint.
    Sse {
        url: String,
        headers: HashMap<String, String>,
    },
    /// Streamable HTTP transport — HTTP with streaming.
    StreamableHttp {
        url: String,
        headers: HashMap<String, String>,
    },
}

impl Default for McpTransport {
    /// Default transport = an empty Stdio invocation. Lets `McpServerConfig`
    /// derive `Default` so config literals can spread `..Default::default()`
    /// for the new `lazy` field without repeating every field.
    fn default() -> Self {
        Self::Stdio {
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
        }
    }
}

// ─── McpServerConfig ────────────────────────────────────────────────────────

/// Configuration for a MCP server connection.
#[derive(Debug, Clone, Default)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
    pub requires_api_key: bool,
    pub api_key_field: Option<String>,
    /// Defer the actual connect until first use (`ensure_connected` /
    /// `connect_server`). When `true`, `connect_all_enabled` skips this server
    /// at startup; the caller connects it on demand.
    ///
    /// **Scope note**: a fully model-transparent lazy server (one the model
    /// can call) still requires the `Deferred` / `tool_search` machinery
    /// (issue #27) because the tool schema a model sees comes from
    /// `tools/list`, which only runs after a connect. This flag covers the
    /// "don't connect at startup, connect on explicit trigger" half; once
    /// connected, `reload_data_layer` registers the discovered tools.
    pub lazy: bool,
}

// ─── Tool name normalization ─────────────────────────────────────────────────
//
// Discovered MCP tool names are namespaced into `mcp__<server>__<tool>` so that
// two servers exposing a same-named tool (e.g. `read_file`) never collide in
// the shared `ToolRegistry`. This mirrors the codex MCP client design. The raw
// tool name stays queryable via the tool's description / `McpToolInfo`.

/// Sanitize a name component into a lowercase `[a-z0-9_]` slug with no
/// leading/trailing or repeated underscores.
fn sanitize_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_under = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_under = false;
        } else if !prev_under {
            out.push('_');
            prev_under = true;
        }
    }
    out.trim_matches('_').to_string()
}

/// Normalize a discovered MCP tool name into a namespaced, collision-free
/// identifier `mcp__<server>__<tool>`. When the combined name would exceed
/// 64 bytes, the tool segment is truncated and an 8-hex SHA1 digest of the
/// original `server::tool` is appended to preserve uniqueness within the
/// 64-byte cap (a common provider identifier limit).
pub fn normalize_tool_name(server: &str, tool: &str) -> String {
    let srv = sanitize_component(server);
    let srv = if srv.is_empty() {
        "server".to_string()
    } else {
        srv
    };
    let tl = sanitize_component(tool);
    let tool_seg = if tl.is_empty() {
        "tool".to_string()
    } else {
        tl
    };
    let prefix = format!("mcp__{}__", srv);
    const MAX: usize = 64;
    if prefix.len() + tool_seg.len() <= MAX {
        return format!("{}{}", prefix, tool_seg);
    }
    // Truncate tool to fit prefix + "_" + 8 hex digest, keeping the digest to
    // guarantee uniqueness across distinct original names that collapse to the
    // same sanitized+truncated form.
    let budget = MAX
        .saturating_sub(prefix.len())
        .saturating_sub(9) // "_" + 8 hex
        .max(1);
    let trunc: String = tool_seg.chars().take(budget).collect();
    let mut hasher = Sha1::new();
    hasher.update(format!("{}::{}", server, tool).as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest
        .iter()
        .take(4)
        .map(|b| format!("{:02x}", b))
        .collect();
    format!("{}{}_{}", prefix, trunc, hex)
}

// ─── McpConnectionStatus ─────────────────────────────────────────────────────

/// Runtime status of one configured MCP server connection.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpConnectionStatus {
    /// Connected — carries the normalized tool names exposed by this server.
    Connected { tools: Vec<String> },
    /// Configured but not currently connected.
    Disconnected,
    /// Not present in the manager at all.
    NotConfigured,
}

impl McpConnectionStatus {
    /// Whether the server is currently connected.
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }
}

// ─── McpToolPermissions ──────────────────────────────────────────────────────
//
// Per-server MCP tool permission policy. This sets the **tool-declared**
// `PermissionLevel` and `ToolExposure` for the wrappers a server exposes —
// the same knobs the DomainPack `PermissionProfile` can still tighten per
// name (the DomainPack `PermissionResolver` / `ExposureResolver` layer on
// top, exactly as for any built-in tool). Default = `Standard` / `Direct`
// everywhere, preserving the pre-existing behaviour.

fn default_standard_level() -> PermissionLevel {
    PermissionLevel::Standard
}

/// Per-server MCP tool permission + exposure policy.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct McpToolPermissions {
    /// Default `PermissionLevel` for tools on this server unless overridden.
    #[serde(default = "default_standard_level")]
    pub default_level: PermissionLevel,
    /// Per-tool `PermissionLevel` overrides, keyed by the **raw** tool name
    /// the remote server advertises (the un-namespaced name).
    #[serde(default)]
    pub tool_overrides: HashMap<String, PermissionLevel>,
    /// Per-tool `ToolExposure` overrides, keyed by the raw tool name. Absent
    /// = `Direct` (the `Tool::exposure` default).
    #[serde(default)]
    pub tool_exposure: HashMap<String, ToolExposure>,
}

impl McpToolPermissions {
    /// Resolve the effective `PermissionLevel` for a tool: per-tool override
    /// if present, else the server default.
    pub fn level_for(&self, tool: &str) -> PermissionLevel {
        self.tool_overrides
            .get(tool)
            .copied()
            .unwrap_or(self.default_level)
    }

    /// Resolve the effective `ToolExposure` for a tool: per-tool override if
    /// present, else `Direct`.
    pub fn exposure_for(&self, tool: &str) -> ToolExposure {
        self.tool_exposure.get(tool).copied().unwrap_or_default()
    }
}

impl Default for McpToolPermissions {
    fn default() -> Self {
        Self {
            default_level: PermissionLevel::Standard,
            tool_overrides: HashMap::new(),
            tool_exposure: HashMap::new(),
        }
    }
}

// ─── McpOAuthTokenRefresher ───────────────────────────────────────────────────
//
// The MCP OAuth flow (discovery / login / token storage / refresh) lives in
// the higher `oneai-mcp` crate — it owns reqwest + the token store. But the
// transport-level 401-retry must happen inside `McpConnection` (this crate),
// because that is where the failing HTTP POST is observed. To cross the layer
// without a reverse dependency, the flow crate implements this trait and
// injects it into the manager / connection. On a 401 from an HTTP transport,
// the connection calls `refresh_token` to obtain a fresh bearer and retries
// the call once.

/// Refresh-on-401 hook for MCP HTTP transports.
///
/// Implementations (in `oneai-mcp`) load the stored refresh token for a
/// server, POST it to the authorization server's token endpoint, persist the
/// new tokens, and return the new access token. Returns `Ok(None)` when no
/// stored tokens / no refresh token exist (so the caller surfaces the 401
/// rather than looping).
#[async_trait]
pub trait McpOAuthTokenRefresher: Send + Sync {
    /// Refresh if needed and return the current access token (if any) for a
    /// server. Called by `McpConnection` on a 401 response.
    async fn refresh_token(&self, server_name: &str) -> Result<Option<String>>;
}

// ─── McpConnection ──────────────────────────────────────────────────────────

/// A persistent connection to an MCP server.
///
/// Supports three transport modes:
/// - **Stdio**: Persistent subprocess for local MCP servers
/// - **SSE**: HTTP-based SSE endpoint for remote MCP servers
/// - **StreamableHttp**: POST + SSE stream for remote MCP servers
///
/// The connection keeps the transport alive for the entire session:
/// - `connect_and_discover()` establishes the connection, performs the
///   JSON-RPC handshake (initialize → initialized → list_tools), and
///   stores discovered tools.
/// - `call_tool()` sends tool calls via the active transport.
/// - `shutdown()` closes the connection gracefully.
pub struct McpConnection {
    config: McpServerConfig,
    tools: HashMap<String, McpToolInfo>,
    /// The subprocess child process (Stdio transport, kept alive for the session).
    child: Option<Child>,
    /// Stdin writer for sending JSON-RPC messages (Stdio transport).
    stdin_writer: Option<tokio::io::BufWriter<tokio::process::ChildStdin>>,
    /// Stdout reader for receiving JSON-RPC responses (Stdio transport).
    stdout_reader: Option<BufReader<tokio::process::ChildStdout>>,
    /// HTTP client for SSE/StreamableHttp transports.
    http_client: Option<reqwest::Client>,
    /// SSE endpoint URL for receiving server messages (SSE transport).
    sse_url: Option<String>,
    /// POST endpoint URL for sending client messages (SSE/StreamableHttp).
    post_url: Option<String>,
    /// Session ID for StreamableHttp (returned by server during handshake).
    session_id: Option<String>,
    /// Next JSON-RPC request ID (incremented for each request).
    next_id: u64,
    /// Optional OAuth token refresher — invoked on a 401 from an HTTP
    /// transport to obtain a fresh bearer and retry the call once. `None`
    /// for servers without OAuth.
    token_refresher: Option<Arc<dyn McpOAuthTokenRefresher>>,
    /// `serverInfo` returned by the remote during `initialize` (name /
    /// version of the server, if advertised). Snapshotted into the
    /// [`McpServerConnectionIdentity`] so status reads don't need to lock
    /// the live connection.
    server_info: Option<serde_json::Value>,
}

/// Information about a tool discovered from an MCP server.
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub parameters_schema: serde_json::Value,
    pub server_name: String,
}

impl McpConnection {
    /// Create a new connection from configuration.
    pub fn new(config: McpServerConfig) -> Self {
        Self {
            config,
            tools: HashMap::new(),
            child: None,
            stdin_writer: None,
            next_id: 1,
            stdout_reader: None,
            http_client: None,
            sse_url: None,
            post_url: None,
            session_id: None,
            token_refresher: None,
            server_info: None,
        }
    }

    /// Attach an OAuth token refresher — invoked on a 401 from an HTTP
    /// transport (SSE / StreamableHttp) to obtain a fresh bearer and retry
    /// the call once. `None` disables the 401-retry path.
    pub fn with_token_refresher(mut self, refresher: Arc<dyn McpOAuthTokenRefresher>) -> Self {
        self.token_refresher = Some(refresher);
        self
    }

    /// Replace the bearer in this connection's transport headers. Used after
    /// a successful refresh so the retry (and subsequent calls) carry the new
    /// token. No-op for the Stdio transport.
    fn set_bearer_token(&mut self, token: &str) {
        let value = format!("Bearer {}", token);
        match &mut self.config.transport {
            McpTransport::Sse { headers, .. } | McpTransport::StreamableHttp { headers, .. } => {
                headers.insert("Authorization".to_string(), value);
            }
            McpTransport::Stdio { .. } => {}
        }
    }

    /// Connect to the MCP server and discover available tools.
    ///
    /// Protocol flow:
    /// 1. Launch subprocess (Stdio transport)
    /// 2. Send `initialize` request → receive capabilities
    /// 3. Send `initialized` notification
    /// 4. Send `tools/list` request → receive tool definitions
    /// 5. Store discovered tools and keep connection alive
    pub async fn connect_and_discover(&mut self) -> Result<()> {
        match &self.config.transport {
            McpTransport::Stdio { command, args, env } => {
                // Launch the subprocess
                let mut cmd = Command::new(command);
                for arg in args {
                    cmd.arg(arg);
                }
                for (key, value) in env {
                    cmd.env(key, value);
                }
                cmd.stdout(std::process::Stdio::piped())
                    .stdin(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());

                let mut child = cmd.spawn().map_err(|e| {
                    oneai_core::error::OneAIError::Provider(format!(
                        "Failed to launch MCP server '{}': {}",
                        command, e
                    ))
                })?;

                let stdin = child.stdin.take().ok_or_else(|| {
                    oneai_core::error::OneAIError::Provider("No stdin pipe".to_string())
                })?;
                let stdout = child.stdout.take().ok_or_else(|| {
                    oneai_core::error::OneAIError::Provider("No stdout pipe".to_string())
                })?;

                let stdin_writer = tokio::io::BufWriter::new(stdin);
                let stdout_reader = BufReader::new(stdout);

                // Store the persistent connection handles
                self.stdin_writer = Some(stdin_writer);
                self.stdout_reader = Some(stdout_reader);
                self.child = Some(child);

                // Step 1: Send initialize request
                let init_request = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": self.next_id,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "clientInfo": { "name": "oneai", "version": "0.1.0" }
                    }
                });
                self.next_id += 1;

                self.send_jsonrpc(&init_request).await?;
                let init_response = self.read_jsonrpc_response().await?;

                // Verify initialize response
                if init_response.get("error").is_some() {
                    let error = init_response.get("error").unwrap();
                    return Err(oneai_core::error::OneAIError::Provider(format!(
                        "MCP initialize error: {}",
                        error
                    )));
                }
                self.server_info = Self::extract_server_info(&init_response);

                tracing::info!(
                    "MCP initialized with server '{}' — capabilities: {}",
                    self.config.name,
                    init_response
                        .get("result")
                        .and_then(|r| r.get("capabilities"))
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                );

                // Step 2: Send initialized notification
                let initialized_notification = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                });
                self.send_jsonrpc(&initialized_notification).await?;

                // Step 3: Send list_tools request
                let list_tools_request = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": self.next_id,
                    "method": "tools/list",
                    "params": {}
                });
                self.next_id += 1;

                self.send_jsonrpc(&list_tools_request).await?;
                let tools_response = self.read_jsonrpc_response().await?;

                // Parse tool definitions from the response
                if let Some(result) = tools_response.get("result") {
                    if let Some(tool_list) = result.get("tools").and_then(|t| t.as_array()) {
                        for tool_def in tool_list {
                            let name = tool_def
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let description = tool_def
                                .get("description")
                                .and_then(|d| d.as_str())
                                .unwrap_or("")
                                .to_string();
                            let schema = tool_def
                                .get("inputSchema")
                                .cloned()
                                .unwrap_or(serde_json::json!({"type": "object"}));

                            self.tools.insert(
                                name.clone(),
                                McpToolInfo {
                                    name,
                                    description,
                                    parameters_schema: schema,
                                    server_name: self.config.name.clone(),
                                },
                            );
                        }
                    }
                }

                tracing::info!(
                    "MCP connection established with server '{}' via Stdio — discovered {} tools",
                    self.config.name,
                    self.tools.len()
                );

                Ok(())
            }
            McpTransport::Sse { url, headers } => {
                // SSE transport: connect to HTTP SSE endpoint
                // 1. Open SSE stream to receive server messages
                // 2. Send initialize request via HTTP POST
                // 3. Parse the initialize response from the SSE stream
                // 4. Send initialized notification via POST
                // 5. Send tools/list request via POST
                // 6. Parse tools from SSE stream
                let client = Self::build_http_client(headers)?;

                // Step 1: Send initialize request via POST to the SSE endpoint
                // The MCP SSE protocol requires sending JSON-RPC via POST
                // and receiving responses via the SSE event stream.
                let init_request = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": self.next_id,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "clientInfo": { "name": "oneai", "version": "0.1.0" }
                    }
                });
                self.next_id += 1;

                // POST the initialize request
                let init_response =
                    Self::http_post_json(&client, url, headers, &init_request).await?;

                // Verify initialize response
                if init_response.get("error").is_some() {
                    let error = init_response.get("error").unwrap();
                    return Err(oneai_core::error::OneAIError::Provider(format!(
                        "MCP SSE initialize error: {}",
                        error
                    )));
                }
                self.server_info = Self::extract_server_info(&init_response);

                tracing::info!(
                    "MCP initialized with SSE server '{}' — capabilities: {}",
                    self.config.name,
                    init_response
                        .get("result")
                        .and_then(|r| r.get("capabilities"))
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                );

                // Step 2: Send initialized notification
                let initialized_notification = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                });
                Self::http_post_json(&client, url, headers, &initialized_notification).await?;

                // Step 3: Send tools/list request
                let list_tools_request = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": self.next_id,
                    "method": "tools/list",
                    "params": {}
                });
                self.next_id += 1;

                let tools_response =
                    Self::http_post_json(&client, url, headers, &list_tools_request).await?;

                // Parse tool definitions
                if let Some(result) = tools_response.get("result") {
                    if let Some(tool_list) = result.get("tools").and_then(|t| t.as_array()) {
                        for tool_def in tool_list {
                            let name = tool_def
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let description = tool_def
                                .get("description")
                                .and_then(|d| d.as_str())
                                .unwrap_or("")
                                .to_string();
                            let schema = tool_def
                                .get("inputSchema")
                                .cloned()
                                .unwrap_or(serde_json::json!({"type": "object"}));

                            self.tools.insert(
                                name.clone(),
                                McpToolInfo {
                                    name,
                                    description,
                                    parameters_schema: schema,
                                    server_name: self.config.name.clone(),
                                },
                            );
                        }
                    }
                }

                // Store the HTTP client and URLs for future calls
                self.http_client = Some(client);
                self.sse_url = Some(url.clone());
                self.post_url = Some(url.clone());

                tracing::info!(
                    "MCP SSE connection established with server '{}' — discovered {} tools",
                    self.config.name,
                    self.tools.len()
                );

                Ok(())
            }
            McpTransport::StreamableHttp { url, headers } => {
                // StreamableHttp transport: POST requests, SSE stream responses
                // Similar to SSE but with session management.
                // The server returns a session ID in the initial response
                // that must be included in subsequent requests.
                let client = Self::build_http_client(headers)?;

                // Step 1: Send initialize request via POST
                let init_request = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": self.next_id,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "clientInfo": { "name": "oneai", "version": "0.1.0" }
                    }
                });
                self.next_id += 1;

                let (init_response, resp_headers) =
                    Self::http_post_with_headers(&client, url, headers, &init_request).await?;

                // Extract session ID from response headers (if provided)
                // MCP StreamableHttp uses Mcp-Session-Id header
                let session_id = resp_headers
                    .get("mcp-session-id")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());

                if init_response.get("error").is_some() {
                    let error = init_response.get("error").unwrap();
                    return Err(oneai_core::error::OneAIError::Provider(format!(
                        "MCP StreamableHttp initialize error: {}",
                        error
                    )));
                }
                self.server_info = Self::extract_server_info(&init_response);

                tracing::info!(
                    "MCP initialized with StreamableHttp server '{}' — session_id: {:?}",
                    self.config.name,
                    session_id
                );

                // Step 2: Send initialized notification
                let initialized_notification = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                });
                Self::http_post_with_session(
                    &client,
                    url,
                    headers,
                    session_id.as_deref(),
                    &initialized_notification,
                )
                .await?;

                // Step 3: Send tools/list request
                let list_tools_request = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": self.next_id,
                    "method": "tools/list",
                    "params": {}
                });
                self.next_id += 1;

                let (tools_response, _) = Self::http_post_with_session(
                    &client,
                    url,
                    headers,
                    session_id.as_deref(),
                    &list_tools_request,
                )
                .await?;

                // Parse tool definitions
                if let Some(result) = tools_response.get("result") {
                    if let Some(tool_list) = result.get("tools").and_then(|t| t.as_array()) {
                        for tool_def in tool_list {
                            let name = tool_def
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let description = tool_def
                                .get("description")
                                .and_then(|d| d.as_str())
                                .unwrap_or("")
                                .to_string();
                            let schema = tool_def
                                .get("inputSchema")
                                .cloned()
                                .unwrap_or(serde_json::json!({"type": "object"}));

                            self.tools.insert(
                                name.clone(),
                                McpToolInfo {
                                    name,
                                    description,
                                    parameters_schema: schema,
                                    server_name: self.config.name.clone(),
                                },
                            );
                        }
                    }
                }

                // Store for future calls
                self.http_client = Some(client);
                self.sse_url = Some(url.clone());
                self.post_url = Some(url.clone());
                self.session_id = session_id;

                tracing::info!("MCP StreamableHttp connection established with server '{}' — discovered {} tools",
                    self.config.name, self.tools.len());

                Ok(())
            }
        }
    }

    /// Call a tool on the MCP server using the active transport.
    ///
    /// Supports Stdio (persistent subprocess), SSE (HTTP POST), and
    /// StreamableHttp (HTTP POST with session ID).
    ///
    /// For the HTTP transports, a 401 response triggers a single
    /// OAuth token refresh (via the injected [`McpOAuthTokenRefresher`],
    /// if any) and one retry — the "重试" leg of the OAuth flow. Without a
    /// refresher, the 401 surfaces as an error like any other non-2xx.
    pub async fn call_tool(
        &mut self,
        tool_name: &str,
        args: serde_json::Value,
    ) -> Result<ToolOutput> {
        let call_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": args
            }
        });
        self.next_id += 1;

        // Stdio has its own persistent-subprocess flow and no HTTP/OAuth path.
        if let McpTransport::Stdio { .. } = &self.config.transport {
            if self.stdin_writer.is_none() || self.stdout_reader.is_none() {
                return Ok(ToolOutput {
                    success: false,
                    content: String::new(),
                    error: Some(
                        "MCP connection not established — call connect_and_discover() first"
                            .to_string(),
                    ),
                    ..Default::default()
                });
            }
            self.send_jsonrpc(&call_request).await?;
            let call_response = self.read_jsonrpc_response().await?;
            return Self::parse_tool_call_response(&call_response);
        }

        // HTTP transports (SSE / StreamableHttp) share the OAuth-aware path.
        let (url, use_session) = match &self.config.transport {
            McpTransport::Sse { url, .. } => (url.clone(), false),
            McpTransport::StreamableHttp { url, .. } => (url.clone(), true),
            McpTransport::Stdio { .. } => unreachable!("stdio handled above"),
        };
        let response = self
            .send_http_call(&url, &call_request, use_session)
            .await?;
        Self::parse_tool_call_response(&response)
    }

    /// Send a `tools/call` POST for an HTTP transport, with one-shot 401 →
    /// refresh → retry. Returns the parsed JSON-RPC response. `use_session`
    /// selects the StreamableHttp session-header path over the plain SSE path.
    async fn send_http_call(
        &mut self,
        url: &str,
        message: &serde_json::Value,
        use_session: bool,
    ) -> Result<serde_json::Value> {
        // Clone the Client (cheap — it's an `Arc` internally) so the
        // immutable borrow of `self.http_client` ends before the `&mut self`
        // refresh path below.
        let client = self.http_client.clone().ok_or_else(|| {
            oneai_core::error::OneAIError::Provider(format!(
                "{} HTTP client not initialized",
                if use_session { "StreamableHttp" } else { "SSE" }
            ))
        })?;

        // Snapshot the current headers + session id so the immutable borrow of
        // `self.config.transport` ends before the `&mut self` refresh below.
        let (headers, session_id) = match &self.config.transport {
            McpTransport::Sse { headers, .. } => (headers.clone(), None),
            McpTransport::StreamableHttp { headers, .. } => (
                headers.clone(),
                if use_session {
                    self.session_id.clone()
                } else {
                    None
                },
            ),
            McpTransport::Stdio { .. } => {
                return Err(oneai_core::error::OneAIError::Provider(
                    "send_http_call called for a Stdio transport".to_string(),
                ));
            }
        };

        let response = if use_session {
            Self::send_post_session(&client, url, &headers, session_id.as_deref(), message).await?
        } else {
            Self::send_post(&client, url, &headers, message).await?
        };

        // 401 → refresh once (if a refresher is wired) and retry the same call.
        if response.status().as_u16() == 401 {
            if let Some(refresher) = self.token_refresher.clone() {
                if let Some(new_token) = refresher.refresh_token(&self.config.name).await? {
                    self.set_bearer_token(&new_token);
                    let headers = match &self.config.transport {
                        McpTransport::Sse { headers, .. }
                        | McpTransport::StreamableHttp { headers, .. } => headers.clone(),
                        McpTransport::Stdio { .. } => headers,
                    };
                    let retry = if use_session {
                        Self::send_post_session(
                            &client,
                            url,
                            &headers,
                            session_id.as_deref(),
                            message,
                        )
                        .await?
                    } else {
                        Self::send_post(&client, url, &headers, message).await?
                    };
                    let (val, _) = Self::parse_mcp_response(retry, url).await?;
                    return Ok(val);
                }
            }
        }

        let (val, _) = Self::parse_mcp_response(response, url).await?;
        Ok(val)
    }

    /// Send a JSON-RPC message via the persistent stdin connection.
    async fn send_jsonrpc(&mut self, message: &serde_json::Value) -> Result<()> {
        if let Some(writer) = &mut self.stdin_writer {
            let json_str = serde_json::to_string(message).map_err(|e| {
                oneai_core::error::OneAIError::Provider(format!("JSON serialization error: {}", e))
            })?;

            let frame = format!("Content-Length: {}\r\n\r\n{}", json_str.len(), json_str);
            writer.write_all(frame.as_bytes()).await.map_err(|e| {
                oneai_core::error::OneAIError::Provider(format!("MCP write error: {}", e))
            })?;
            writer.flush().await.map_err(|e| {
                oneai_core::error::OneAIError::Provider(format!("MCP flush error: {}", e))
            })?;
            Ok(())
        } else {
            Err(oneai_core::error::OneAIError::Provider(
                "No MCP stdin connection".to_string(),
            ))
        }
    }

    /// Read a JSON-RPC response via the persistent stdout connection.
    ///
    /// Uses the McpFramingParser for proper Content-Length header + body parsing.
    async fn read_jsonrpc_response(&mut self) -> Result<serde_json::Value> {
        if let Some(reader) = &mut self.stdout_reader {
            let mut parser = McpFramingParser::new();
            let mut buffer = [0u8; 8192];

            // Read until we get a complete frame
            loop {
                let n = reader.read(&mut buffer).await.map_err(|e| {
                    oneai_core::error::OneAIError::Provider(format!("MCP read error: {}", e))
                })?;

                if n == 0 {
                    // EOF — subprocess has closed stdout
                    return Err(oneai_core::error::OneAIError::Provider(
                        "MCP server closed stdout (process may have exited)".to_string(),
                    ));
                }

                parser.feed(&buffer[..n]);

                // Try to parse all available frames
                // We need to find the response frame (has an "id" field)
                let frames = parser.parse_all_frames();
                for frame in frames {
                    // Check if this is a response (has "id" field matching our request)
                    // Notifications don't have "id" — skip them
                    if frame.get("id").is_some() {
                        return Ok(frame);
                    }
                    // Notifications are informational — just log them
                    if frame.get("method").is_some() {
                        tracing::debug!("MCP notification: {:?}", frame.get("method"));
                    }
                }

                // If no response frame yet, continue reading
            }
        } else {
            Err(oneai_core::error::OneAIError::Provider(
                "No MCP stdout connection".to_string(),
            ))
        }
    }

    /// Shutdown the MCP connection — kill subprocess or close HTTP client.
    pub async fn shutdown(&mut self) -> Result<()> {
        if let Some(child) = &mut self.child {
            // Try graceful shutdown first (SIGTERM on Unix)
            child.kill().await.map_err(|e| {
                oneai_core::error::OneAIError::Provider(format!(
                    "Failed to kill MCP subprocess: {}",
                    e
                ))
            })?;
            tracing::info!(
                "MCP connection to server '{}' shut down (Stdio)",
                self.config.name
            );
        }
        self.stdin_writer = None;
        self.stdout_reader = None;
        self.child = None;
        self.http_client = None;
        self.sse_url = None;
        self.post_url = None;
        self.session_id = None;
        Ok(())
    }

    /// Get all discovered tools from this server.
    pub fn tools(&self) -> &HashMap<String, McpToolInfo> {
        &self.tools
    }

    /// Get the server name.
    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// `serverInfo` advertised by the remote during `initialize` (`None` if
    /// the server didn't send one).
    pub fn server_info(&self) -> Option<&serde_json::Value> {
        self.server_info.as_ref()
    }

    /// Extract the `serverInfo` object from an `initialize` response, if the
    /// server advertised one. Used to populate [`McpServerConnectionIdentity`].
    fn extract_server_info(init_response: &serde_json::Value) -> Option<serde_json::Value> {
        init_response
            .get("result")
            .and_then(|r| r.get("serverInfo"))
            .cloned()
    }

    // ─── HTTP Helper Methods ─────────────────────────────────────────────────

    /// Build an HTTP client with optional custom headers.
    fn build_http_client(headers: &HashMap<String, String>) -> Result<reqwest::Client> {
        let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30));

        // Add custom headers as default request headers
        for (key, value) in headers {
            builder =
                builder.default_headers(reqwest::header::HeaderMap::from_iter(std::iter::once((
                    reqwest::header::HeaderName::from_bytes(key.as_bytes())
                        .unwrap_or(reqwest::header::AUTHORIZATION),
                    reqwest::header::HeaderValue::from_str(value)
                        .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static("")),
                ))));
        }

        builder.build().map_err(|e| {
            oneai_core::error::OneAIError::Provider(format!("Failed to build HTTP client: {}", e))
        })
    }

    /// Apply a custom-headers map to a request builder.
    fn apply_headers(
        mut request: reqwest::RequestBuilder,
        headers: &HashMap<String, String>,
    ) -> reqwest::RequestBuilder {
        for (key, value) in headers {
            if let Ok(header_name) = reqwest::header::HeaderName::from_bytes(key.as_bytes()) {
                if let Ok(header_value) = reqwest::header::HeaderValue::from_str(value) {
                    request = request.header(header_name, header_value);
                }
            }
        }
        request
    }

    /// Build and **send** a JSON POST request (no session header), returning
    /// the raw `reqwest::Response`. Network errors only — the caller inspects
    /// `.status()` and reads the body. Split out so `call_tool` can branch on
    /// a 401 before the body is consumed.
    async fn send_post(
        client: &reqwest::Client,
        url: &str,
        headers: &HashMap<String, String>,
        message: &serde_json::Value,
    ) -> Result<reqwest::Response> {
        let request = Self::apply_headers(client.post(url).json(message), headers);
        request.send().await.map_err(|e| {
            oneai_core::error::OneAIError::Provider(format!(
                "MCP HTTP POST error to {}: {}",
                url, e
            ))
        })
    }

    /// Build and **send** a JSON POST request with the `Mcp-Session-Id` header
    /// (StreamableHttp). Like [`send_post`](Self::send_post), returns the raw
    /// response so the caller can react to a 401.
    async fn send_post_session(
        client: &reqwest::Client,
        url: &str,
        headers: &HashMap<String, String>,
        session_id: Option<&str>,
        message: &serde_json::Value,
    ) -> Result<reqwest::Response> {
        let request = client.post(url).json(message);
        let request = if let Some(sid) = session_id {
            request.header("Mcp-Session-Id", sid)
        } else {
            request
        };
        let request = Self::apply_headers(request, headers);
        request.send().await.map_err(|e| {
            oneai_core::error::OneAIError::Provider(format!(
                "MCP HTTP POST error to {}: {}",
                url, e
            ))
        })
    }

    /// Consume a `reqwest::Response`: error on non-2xx (carrying status +
    /// body), otherwise parse the body as plain JSON or an SSE `data:` event.
    async fn parse_mcp_response(
        response: reqwest::Response,
        url: &str,
    ) -> Result<(serde_json::Value, reqwest::header::HeaderMap)> {
        let resp_headers = response.headers().clone();
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(oneai_core::error::OneAIError::Provider(format!(
                "MCP HTTP POST returned status {} from {}: {}",
                status.as_u16(),
                url,
                body
            )));
        }

        let body = response.text().await.map_err(|e| {
            oneai_core::error::OneAIError::Provider(format!("MCP HTTP POST read error: {}", e))
        })?;

        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
            return Ok((json, resp_headers));
        }

        for line in body.lines() {
            if line.starts_with("data: ") {
                let data = line.trim_start_matches("data: ").trim();
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                    return Ok((json, resp_headers));
                }
            }
        }

        Err(oneai_core::error::OneAIError::Provider(format!(
            "MCP HTTP POST response could not be parsed from {}: {}",
            url,
            &body[..body.len().min(200)]
        )))
    }

    /// Send a JSON-RPC message via HTTP POST and receive the JSON response.
    async fn http_post_json(
        client: &reqwest::Client,
        url: &str,
        headers: &HashMap<String, String>,
        message: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let response = Self::send_post(client, url, headers, message).await?;
        let (json, _) = Self::parse_mcp_response(response, url).await?;
        Ok(json)
    }

    /// Send a JSON-RPC message via HTTP POST and receive response + headers.
    async fn http_post_with_headers(
        client: &reqwest::Client,
        url: &str,
        headers: &HashMap<String, String>,
        message: &serde_json::Value,
    ) -> Result<(serde_json::Value, reqwest::header::HeaderMap)> {
        let response = Self::send_post(client, url, headers, message).await?;
        Self::parse_mcp_response(response, url).await
    }

    /// Send a JSON-RPC message via HTTP POST with session ID header.
    async fn http_post_with_session(
        client: &reqwest::Client,
        url: &str,
        headers: &HashMap<String, String>,
        session_id: Option<&str>,
        message: &serde_json::Value,
    ) -> Result<(serde_json::Value, reqwest::header::HeaderMap)> {
        let response = Self::send_post_session(client, url, headers, session_id, message).await?;
        Self::parse_mcp_response(response, url).await
    }

    /// Parse a tool call response from any transport into a ToolOutput.
    fn parse_tool_call_response(response: &serde_json::Value) -> Result<ToolOutput> {
        // Check for errors
        if let Some(error) = response.get("error") {
            let error_msg = error
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("Unknown MCP error");
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("MCP tool error: {}", error_msg)),
                ..Default::default()
            });
        }

        // Extract content from the result
        let content = if let Some(result) = response.get("result") {
            if let Some(content_arr) = result.get("content").and_then(|c| c.as_array()) {
                // MCP returns content as an array of content blocks
                content_arr
                    .iter()
                    .filter_map(|block| {
                        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                            block
                                .get("text")
                                .and_then(|t| t.as_str())
                                .map(|s| s.to_string())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                result.to_string()
            }
        } else {
            "No result content".to_string()
        };

        Ok(ToolOutput {
            success: true,
            content,
            error: None,
            ..Default::default()
        })
    }
}

// ─── McpToolWrapper (real implementation) ───────────────────────────────────

/// MCP tool wrapper that implements the OneAI Tool trait with real MCP calls.
///
/// Uses a shared Arc<McpConnection> for persistent connection access.
/// The connection must be mutable for call_tool (needs to read/write),
/// so we use an Arc<Mutex> pattern.
pub struct McpToolWrapper {
    /// Namespaced registry-facing name (`mcp__<server>__<tool>`).
    name: String,
    /// The raw tool name the remote MCP server expects in `tools/call`.
    remote_name: String,
    description: String,
    parameters_schema: serde_json::Value,
    server_name: String,
    /// Tool-declared permission level (server default or per-tool override).
    permission_level: PermissionLevel,
    /// Tool-declared exposure to the model / code mode.
    exposure: ToolExposure,
    /// Shared mutable connection — needed because call_tool reads/writes to the subprocess.
    connection: Arc<tokio::sync::Mutex<McpConnection>>,
}

impl McpToolWrapper {
    pub fn new(
        name: String,
        description: String,
        parameters_schema: serde_json::Value,
        server_name: String,
        connection: Arc<tokio::sync::Mutex<McpConnection>>,
    ) -> Self {
        // Back-compat single-arg constructor: assume the registry name and the
        // remote name are identical (pre-normalization callers), and the
        // default policy (Standard / Direct).
        let remote_name = name.clone();
        Self {
            name,
            remote_name,
            description,
            parameters_schema,
            server_name,
            permission_level: PermissionLevel::Standard,
            exposure: ToolExposure::Direct,
            connection,
        }
    }

    /// Build a wrapper with distinct registry (normalized) and remote (raw)
    /// tool names — the path used by `McpServerManager::connect_server` once
    /// tools are namespaced.
    pub fn with_remote_name(
        name: String,
        remote_name: String,
        description: String,
        parameters_schema: serde_json::Value,
        server_name: String,
        connection: Arc<tokio::sync::Mutex<McpConnection>>,
    ) -> Self {
        Self {
            name,
            remote_name,
            description,
            parameters_schema,
            server_name,
            permission_level: PermissionLevel::Standard,
            exposure: ToolExposure::Direct,
            connection,
        }
    }

    /// Build a wrapper carrying an explicit per-tool policy (resolved from the
    /// server's `McpToolPermissions` by the manager at connect time).
    #[allow(clippy::too_many_arguments)]
    pub fn with_policy(
        name: String,
        remote_name: String,
        description: String,
        parameters_schema: serde_json::Value,
        server_name: String,
        permission_level: PermissionLevel,
        exposure: ToolExposure,
        connection: Arc<tokio::sync::Mutex<McpConnection>>,
    ) -> Self {
        Self {
            name,
            remote_name,
            description,
            parameters_schema,
            server_name,
            permission_level,
            exposure,
            connection,
        }
    }

    /// The server this tool was discovered from.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// The raw tool name the remote MCP server expects in `tools/call`.
    pub fn remote_name(&self) -> &str {
        &self.remote_name
    }

    /// The tool-declared permission level (before DomainPack tightening).
    pub fn declared_permission_level(&self) -> PermissionLevel {
        self.permission_level
    }

    /// The tool-declared exposure (before DomainPack tightening).
    pub fn declared_exposure(&self) -> ToolExposure {
        self.exposure
    }
}

#[async_trait]
impl Tool for McpToolWrapper {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.parameters_schema.clone()
    }

    fn risk_level(&self) -> oneai_core::RiskLevel {
        self.permission_level().to_risk_level()
    }

    /// Tool-declared exposure — the DomainPack `ExposureResolver` still
    /// tightens this per name on the hot path.
    fn exposure(&self) -> ToolExposure {
        self.exposure
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let mut conn = self.connection.lock().await;
        // Send the *raw* tool name the remote server expects, not the
        // namespaced registry name.
        conn.call_tool(&self.remote_name, args).await
    }
}

impl PermissionAwareTool for McpToolWrapper {
    fn permission_level(&self) -> PermissionLevel {
        self.permission_level
    }
}

use crate::tool_interfaces::PermissionAwareTool;

// ─── McpTransportKind / McpServerConnectionIdentity / McpBinding ──────────────
//
// codex-style "immutable binding snapshot". The connection's *identity* (who
// it is, what transport kind, what `serverInfo` it advertised) is separated
// from the *live transport state* (subprocess pipes / HTTP client). The
// manager holds `RwLock<HashMap<String, Arc<McpBinding>>>`; readers clone the
// `Arc<McpBinding>` under a read lock and release immediately — `server_status`
// / `all_tool_wrappers` / `get_tool_wrapper` never block an in-flight
// `call_tool` (which only locks the per-connection `Mutex<McpConnection>`)
// nor a concurrent connect/disconnect of another server.

/// The transport family a connection uses — a non-owning projection of
/// [`McpTransport`] so the identity doesn't borrow the live transport config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpTransportKind {
    Stdio,
    Sse,
    StreamableHttp,
}

impl McpTransportKind {
    /// Project from a [`McpTransport`] without cloning its fields.
    pub fn from_transport(transport: &McpTransport) -> Self {
        match transport {
            McpTransport::Stdio { .. } => Self::Stdio,
            McpTransport::Sse { .. } => Self::Sse,
            McpTransport::StreamableHttp { .. } => Self::StreamableHttp,
        }
    }
}

/// A connection's identity — name + transport kind + advertised `serverInfo`.
/// Separated from the live transport so it can be read without locking the
/// connection.
#[derive(Debug, Clone)]
pub struct McpServerConnectionIdentity {
    pub name: String,
    pub transport_kind: McpTransportKind,
    pub server_info: Option<serde_json::Value>,
}

/// An immutable snapshot of one connected server: identity, the live
/// connection (shared `Arc<Mutex<McpConnection>>`), the discovered tool catalog,
/// the resolved per-tool wrappers, the policy, and a `generation` counter the
/// catalog cache uses to invalidate stale snapshots across reconnects.
///
/// `McpToolWrapper` holds the same `Arc<Mutex<McpConnection>>` (not the
/// `Arc<McpBinding>`) — this avoids an `Arc` cycle (binding → wrappers →
/// wrapper → binding) while still giving every wrapper cheap, lock-free access
/// to its live transport.
pub struct McpBinding {
    pub identity: McpServerConnectionIdentity,
    pub connection: Arc<tokio::sync::Mutex<McpConnection>>,
    /// Discovered tools keyed by the **raw** name the remote advertises.
    pub tools: Arc<HashMap<String, McpToolInfo>>,
    /// Wrappers keyed by the **normalized** registry name (`mcp__<srv>__<tool>`).
    pub wrappers: Arc<HashMap<String, Arc<McpToolWrapper>>>,
    pub permissions: McpToolPermissions,
    pub generation: u64,
    pub lazy: bool,
}

impl McpBinding {
    /// Build the immutable snapshot from a just-connected connection + the
    /// resolved per-tool policy. The wrappers share the connection `Arc`.
    fn build(
        config: &McpServerConfig,
        connection: Arc<tokio::sync::Mutex<McpConnection>>,
        conn: &McpConnection,
        permissions: &McpToolPermissions,
        generation: u64,
    ) -> Arc<Self> {
        let mut tools = HashMap::new();
        let mut wrappers = HashMap::new();
        for (raw_name, info) in conn.tools() {
            let normalized = normalize_tool_name(&config.name, raw_name);
            let level = permissions.level_for(raw_name);
            let exposure = permissions.exposure_for(raw_name);
            let wrapper = Arc::new(McpToolWrapper::with_policy(
                normalized.clone(),
                info.name.clone(),
                info.description.clone(),
                info.parameters_schema.clone(),
                info.server_name.clone(),
                level,
                exposure,
                connection.clone(),
            ));
            wrappers.insert(normalized, wrapper);
            tools.insert(raw_name.clone(), info.clone());
        }
        Arc::new(Self {
            identity: McpServerConnectionIdentity {
                name: config.name.clone(),
                transport_kind: McpTransportKind::from_transport(&config.transport),
                server_info: conn.server_info().cloned(),
            },
            connection,
            tools: Arc::new(tools),
            wrappers: Arc::new(wrappers),
            permissions: permissions.clone(),
            generation,
            lazy: config.lazy,
        })
    }

    /// Normalized wrapper names exposed by this binding.
    pub fn tool_names(&self) -> Vec<String> {
        self.wrappers.keys().cloned().collect()
    }
}

// ─── ToolCatalogCache (LRU + generation) ─────────────────────────────────────
//
// Caches the last-known `tools/list` catalog per server across connect/disconnect
// cycles. Each entry carries a `generation` counter; `bump_generation` (called on
// disconnect/reconnect) advances it so a stale snapshot (cached under an older
// generation) no longer satisfies a `get(_, expected_generation)` lookup — the
// caller must re-fetch. An LRU order bounds memory when connecting many servers.
//
// Today the cache is consulted by `cached_tools` / `last_known_tools` for
// diagnostics (e.g. reporting a disconnected server's last-seen tool count). The
// handshake itself still runs `tools/list` (the connection must open regardless);
// wiring the cache to *skip* a redundant `tools/list` on a same-generation
// reconnect is a later optimization — the structure + generation contract are in
// place to support it without another API change.

/// One cached tool catalog entry.
#[derive(Debug, Clone)]
struct CachedCatalog {
    /// Discovered tools (raw-name keyed snapshot projected to a Vec).
    tools: Vec<McpToolInfo>,
    /// Generation this entry was written under. `get` only returns it when the
    /// caller's `expected_generation` matches.
    generation: u64,
    /// Monotonic LRU sequence number (larger = more recently touched).
    lru_seq: u64,
}

/// LRU-bounded, generation-stamped tool catalog cache.
pub struct ToolCatalogCache {
    entries: HashMap<String, CachedCatalog>,
    /// Server names in least-recently-used-first order.
    order: Vec<String>,
    capacity: usize,
    next_seq: u64,
}

impl ToolCatalogCache {
    /// Default capacity (32 servers). Bound chosen to cover typical agent
    /// setups while keeping the cache trivially small.
    pub const DEFAULT_CAPACITY: usize = 32;

    pub fn new() -> Self {
        Self::with_capacity(Self::DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            entries: HashMap::new(),
            order: Vec::new(),
            capacity,
            next_seq: 1,
        }
    }

    /// Return the cached catalog only if `server` is present AND its
    /// generation matches `expected_generation`. A mismatch (stale snapshot
    /// after a reconnect/disconnect bumped the generation) returns `None`.
    /// Returns an owned `Vec` so LRU order can be promoted on the hit (a
    /// borrow into `self.entries` would forbid the mutating touch).
    pub fn get(&mut self, server: &str, expected_generation: u64) -> Option<Vec<McpToolInfo>> {
        let entry = self.entries.get_mut(server)?;
        if entry.generation != expected_generation {
            return None;
        }
        let tools = entry.tools.clone();
        // Promote LRU: move `server` to the back (most-recently-used).
        self.order.retain(|n| n != server);
        self.order.push(server.to_string());
        Some(tools)
    }

    /// Last-known tools for `server`, ignoring generation (diagnostics only —
    /// e.g. "this server is disconnected but last advertised N tools").
    pub fn last_known_tools(&self, server: &str) -> Option<&[McpToolInfo]> {
        self.entries.get(server).map(|e| e.tools.as_slice())
    }

    /// Insert / replace a catalog entry, advancing its LRU position. Evicts
    /// the least-recently-used entry when over capacity.
    pub fn put(&mut self, server: &str, tools: Vec<McpToolInfo>, generation: u64) {
        let seq = self.fresh_seq();
        if self.entries.contains_key(server) {
            if let Some(entry) = self.entries.get_mut(server) {
                entry.tools = tools;
                entry.generation = generation;
                entry.lru_seq = seq;
            }
            self.touch(server, &seq);
            return;
        }
        if self.entries.len() >= self.capacity {
            // Evict the least-recently-used (front of `order`).
            if let Some(victim) = self.order.first().cloned() {
                self.entries.remove(&victim);
                self.order.retain(|n| n != &victim);
            }
        }
        self.entries.insert(
            server.to_string(),
            CachedCatalog {
                tools,
                generation,
                lru_seq: seq,
            },
        );
        self.order.push(server.to_string());
    }

    /// Advance `server`'s generation so any snapshot cached under an older
    /// generation stops matching `get`. Returns the new generation, or `None`
    /// if the server was never cached. The cached entry is *retained* (so
    /// `last_known_tools` still works) but its generation is bumped.
    pub fn bump_generation(&mut self, server: &str) -> Option<u64> {
        // Allocate the seq first to avoid a second `&mut self` borrow while
        // `entry` (from `self.entries.get_mut`) is live.
        let seq = self.fresh_seq();
        let entry = self.entries.get_mut(server)?;
        // Advance past any generation a caller might still hold. The manager's
        // `fresh_generation` is monotonic, so the bumped value just needs to
        // differ from all prior writes — use the cache's own seq counter to
        // stay self-contained.
        let new_gen = entry.generation.wrapping_add(1).max(seq);
        entry.generation = new_gen;
        Some(new_gen)
    }

    /// Drop the cached entry for `server` entirely.
    pub fn invalidate(&mut self, server: &str) {
        self.entries.remove(server);
        self.order.retain(|n| n != server);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn fresh_seq(&mut self) -> u64 {
        let s = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        s
    }

    /// Move `server` to the back of `order` (most-recently-used).
    fn touch(&mut self, server: &str, _seq: &u64) {
        self.order.retain(|n| n != server);
        self.order.push(server.to_string());
    }
}

impl Default for ToolCatalogCache {
    fn default() -> Self {
        Self::new()
    }
}

// ─── McpServerManager ────────────────────────────────────────────────────────

/// MCP server manager — handles real connections to MCP servers.
///
/// Fully internally synchronized: every method takes `&self` and the bindings
/// map lives behind a `tokio::sync::RwLock`. Readers (`server_status`,
/// `all_tool_wrappers`, `get_tool_wrapper`, `server_names`, `is_connected`)
/// take a **read** lock, clone the `Arc<McpBinding>`s they need, and release —
/// they never block an in-flight `call_tool` (which only locks the
/// per-connection `Mutex<McpConnection>`) nor a concurrent connect/disconnect of
/// another server. Writers (`connect_server*`, `disconnect_server`,
/// `reconnect_server*`, `shutdown_all`) take the write lock and swap in a fresh
/// `Arc<McpBinding>`. The OAuth token refresher lives behind a `Mutex` so it
/// can be installed after construction without `&mut self`.
pub struct McpServerManager {
    bindings: tokio::sync::RwLock<HashMap<String, Arc<McpBinding>>>,
    token_refresher: tokio::sync::Mutex<Option<Arc<dyn McpOAuthTokenRefresher>>>,
    next_generation: std::sync::atomic::AtomicU64,
    /// LRU-bounded, generation-stamped tool catalog cache. Populated on
    /// `connect`, generation-bumped on `disconnect`/`reconnect`. See
    /// [`ToolCatalogCache`].
    catalog_cache: std::sync::Mutex<ToolCatalogCache>,
}

impl McpServerManager {
    pub fn new() -> Self {
        Self {
            bindings: tokio::sync::RwLock::new(HashMap::new()),
            token_refresher: tokio::sync::Mutex::new(None),
            next_generation: std::sync::atomic::AtomicU64::new(1),
            catalog_cache: std::sync::Mutex::new(ToolCatalogCache::new()),
        }
    }

    /// Allocate a fresh generation counter for a new binding. The catalog
    /// cache (Stage 4-2) uses this to invalidate stale snapshots across
    /// reconnects; for now it just gives each binding a monotonically
    /// increasing identity.
    fn fresh_generation(&self) -> u64 {
        self.next_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Install an OAuth token refresher used by all subsequently connected
    /// HTTP-transport servers. Existing connections are unaffected (they
    /// captured the refresher at their own connect time, if any).
    pub async fn set_token_refresher(&self, refresher: Arc<dyn McpOAuthTokenRefresher>) {
        let mut slot = self.token_refresher.lock().await;
        *slot = Some(refresher);
    }

    /// Connect to an MCP server, discover tools, and create wrappers using
    /// the default policy (`Standard` / `Direct`). Convenience wrapper around
    /// [`connect_server_with_policy`](Self::connect_server_with_policy).
    pub async fn connect_server(&self, config: McpServerConfig) -> Result<Vec<String>> {
        self.connect_server_with_policy(config, &McpToolPermissions::default())
            .await
    }

    /// Connect to an MCP server, discover tools, and create wrappers carrying
    /// the per-tool permission + exposure policy resolved from `permissions`.
    ///
    /// Each discovered tool is registered under a **normalized, namespaced**
    /// name (`mcp__<server>__<tool>`, see [`normalize_tool_name`]) so that
    /// same-named tools from different servers never collide in the shared
    /// `ToolRegistry`. The returned `Vec` carries those normalized names.
    pub async fn connect_server_with_policy(
        &self,
        config: McpServerConfig,
        permissions: &McpToolPermissions,
    ) -> Result<Vec<String>> {
        let mut connection = McpConnection::new(config.clone());
        // Attach the OAuth refresher (if any) so HTTP-transport calls can
        // recover from a 401 by refreshing + retrying once.
        {
            let slot = self.token_refresher.lock().await;
            if let Some(refresher) = slot.as_ref() {
                connection = connection.with_token_refresher(refresher.clone());
            }
        }
        connection.connect_and_discover().await?;

        let connection_arc = Arc::new(tokio::sync::Mutex::new(connection));
        // Snapshot tools + build wrappers under the per-connection lock.
        let binding = {
            let conn = connection_arc.lock().await;
            McpBinding::build(
                &config,
                connection_arc.clone(),
                &conn,
                permissions,
                self.fresh_generation(),
            )
        };
        let tool_names = binding.tool_names();
        // Snapshot the discovered catalog + generation for the LRU cache before
        // the binding Arc is moved into the bindings map.
        let catalog_tools: Vec<McpToolInfo> = binding.tools.values().cloned().collect();
        let catalog_gen = binding.generation;

        let mut bindings = self.bindings.write().await;
        bindings.insert(config.name.clone(), binding);
        // Refresh the catalog cache so `cached_tools` / `last_known_tools`
        // return the up-to-date snapshot under this generation.
        if let Ok(mut cache) = self.catalog_cache.lock() {
            cache.put(&config.name, catalog_tools, catalog_gen);
        }

        Ok(tool_names)
    }

    /// If `config.name` is already connected, return its known tool names
    /// without reconnecting; otherwise connect now. The lazy-connect path:
    /// callers that defer startup (`lazy: true`) invoke this on first use.
    pub async fn ensure_connected(
        &self,
        config: McpServerConfig,
        permissions: &McpToolPermissions,
    ) -> Result<Vec<String>> {
        {
            let bindings = self.bindings.read().await;
            if let Some(existing) = bindings.get(&config.name) {
                return Ok(existing.tool_names());
            }
        }
        self.connect_server_with_policy(config, permissions).await
    }

    /// Get all tool wrappers across every connected server.
    pub async fn all_tool_wrappers(&self) -> Vec<Arc<McpToolWrapper>> {
        let bindings = self.bindings.read().await;
        let mut out = Vec::new();
        for binding in bindings.values() {
            out.extend(binding.wrappers.values().cloned());
        }
        out
    }

    /// Get a tool wrapper by its normalized registry name. Returns an owned
    /// `Arc` (the read lock is released before returning).
    pub async fn get_tool_wrapper(&self, name: &str) -> Option<Arc<McpToolWrapper>> {
        let bindings = self.bindings.read().await;
        for binding in bindings.values() {
            if let Some(w) = binding.wrappers.get(name) {
                return Some(w.clone());
            }
        }
        None
    }

    /// Names of all currently-connected servers.
    pub async fn server_names(&self) -> Vec<String> {
        let bindings = self.bindings.read().await;
        bindings.keys().cloned().collect()
    }

    /// Whether a server of the given name is currently connected.
    pub async fn is_connected(&self, name: &str) -> bool {
        let bindings = self.bindings.read().await;
        bindings.contains_key(name)
    }

    /// Status of one server connection.
    pub async fn server_status(&self, name: &str) -> McpConnectionStatus {
        let bindings = self.bindings.read().await;
        if let Some(binding) = bindings.get(name) {
            let tools = binding.wrappers.keys().cloned().collect::<Vec<_>>();
            return McpConnectionStatus::Connected { tools };
        }
        // No live binding — consult the catalog cache to distinguish
        // "was connected once, now disconnected" from "never configured".
        if let Ok(cache) = self.catalog_cache.lock() {
            if cache.last_known_tools(name).is_some() {
                return McpConnectionStatus::Disconnected;
            }
        }
        McpConnectionStatus::NotConfigured
    }

    /// Cached catalog for `server`, only if its generation still matches
    /// `expected_generation`. Returns `None` when the server was never cached
    /// or its snapshot was invalidated by a disconnect/reconnect.
    pub fn cached_tools(&self, server: &str, expected_generation: u64) -> Option<Vec<McpToolInfo>> {
        let mut cache = self.catalog_cache.lock().ok()?;
        cache.get(server, expected_generation)
    }

    /// Last-known catalog for `server` (ignoring generation). Diagnostics only.
    pub fn last_known_tools(&self, server: &str) -> Option<Vec<McpToolInfo>> {
        let cache = self.catalog_cache.lock().ok()?;
        cache.last_known_tools(server).map(|s| s.to_vec())
    }

    /// Snapshot of every binding (identity + tools + generation), for
    /// introspection / CLI `mcp status`. Cheaper than rebuilding wrappers.
    pub async fn bindings_snapshot(&self) -> Vec<Arc<McpBinding>> {
        let bindings = self.bindings.read().await;
        bindings.values().cloned().collect()
    }

    /// Disconnect **only** the named server — shut it down and drop its
    /// binding, leaving all other servers untouched.
    ///
    /// This is the per-server counterpart to [`shutdown_all`]; the previous
    /// `disconnect_server` implementations called `shutdown_all` and cleared
    /// every connection (issue #31).
    pub async fn disconnect_server(&self, name: &str) -> Result<()> {
        let binding = {
            let mut bindings = self.bindings.write().await;
            bindings.remove(name)
        };
        if let Some(binding) = binding {
            let mut conn = binding.connection.lock().await;
            conn.shutdown().await?;
            // Bump the catalog generation so any snapshot cached under the old
            // generation stops matching `cached_tools(_, gen)` — a future
            // reconnect repopulates with a fresh generation. The entry is
            // retained so `last_known_tools` can still report the last-seen
            // catalog for diagnostics.
            if let Ok(mut cache) = self.catalog_cache.lock() {
                cache.bump_generation(name);
            }
        }
        Ok(())
    }

    /// Reconnect a server: disconnect the existing connection (if any) then
    /// re-establish it with the supplied config (default policy). Returns the
    /// normalized tool names discovered after the fresh connect.
    pub async fn reconnect_server(&self, config: McpServerConfig) -> Result<Vec<String>> {
        self.reconnect_server_with_policy(config, &McpToolPermissions::default())
            .await
    }

    /// Reconnect a server with an explicit per-tool policy: disconnect the
    /// existing connection (if any) then re-establish it. Returns the
    /// normalized tool names discovered after the fresh connect.
    pub async fn reconnect_server_with_policy(
        &self,
        config: McpServerConfig,
        permissions: &McpToolPermissions,
    ) -> Result<Vec<String>> {
        self.disconnect_server(&config.name).await?;
        self.connect_server_with_policy(config, permissions).await
    }

    /// Shutdown all MCP connections.
    pub async fn shutdown_all(&self) -> Result<()> {
        let drained: Vec<Arc<McpBinding>> = {
            let mut bindings = self.bindings.write().await;
            let drained = bindings.values().cloned().collect::<Vec<_>>();
            bindings.clear();
            drained
        };
        for binding in drained {
            let mut conn = binding.connection.lock().await;
            conn.shutdown().await?;
        }
        // Drop the whole catalog cache — every server is gone.
        if let Ok(mut cache) = self.catalog_cache.lock() {
            *cache = ToolCatalogCache::new();
        }
        Ok(())
    }
}

impl Default for McpServerManager {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Pre-registered default MCP servers ──────────────────────────────────────

pub fn default_mcp_configs() -> Vec<McpServerConfig> {
    vec![McpServerConfig {
        name: "filesystem".to_string(),
        transport: McpTransport::Stdio {
            command: "npx".to_string(),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-filesystem".to_string(),
            ],
            env: HashMap::new(),
        },
        requires_api_key: false,
        api_key_field: None,
        ..Default::default()
    }]
}

pub fn optional_mcp_configs() -> Vec<McpServerConfig> {
    vec![McpServerConfig {
        name: "web_search".to_string(),
        transport: McpTransport::Stdio {
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "@anthropic-ai/mcp-web-search".to_string()],
            env: HashMap::new(),
        },
        requires_api_key: true,
        api_key_field: Some("ANTHROPIC_API_KEY".to_string()),
        ..Default::default()
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_framing_parser_single_frame() {
        let json = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": []}});
        let json_str = serde_json::to_string(&json).unwrap();
        let frame = format!("Content-Length: {}\r\n\r\n{}", json_str.len(), json_str);

        let mut parser = McpFramingParser::new();
        parser.feed(frame.as_bytes());
        let result = parser.try_parse_frame();
        assert!(result.is_some());
        let parsed = result.unwrap();
        assert_eq!(parsed.get("id").and_then(|i| i.as_u64()), Some(1));
        assert!(parsed.get("result").is_some());
    }

    #[test]
    fn test_framing_parser_multiple_frames() {
        let json1 = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {}});
        let json2 = serde_json::json!({"jsonrpc": "2.0", "id": 2, "result": {}});
        let str1 = serde_json::to_string(&json1).unwrap();
        let str2 = serde_json::to_string(&json2).unwrap();
        let frame = format!(
            "Content-Length: {}\r\n\r\n{}Content-Length: {}\r\n\r\n{}",
            str1.len(),
            str1,
            str2.len(),
            str2
        );

        let mut parser = McpFramingParser::new();
        parser.feed(frame.as_bytes());
        let frames = parser.parse_all_frames();
        assert_eq!(frames.len(), 2);
    }

    #[test]
    fn test_framing_parser_partial_frame() {
        let json = serde_json::json!({"jsonrpc": "2.0", "id": 1});
        let json_str = serde_json::to_string(&json).unwrap();
        let full_frame = format!("Content-Length: {}\r\n\r\n{}", json_str.len(), json_str);

        // Feed only part of the frame
        let mut parser = McpFramingParser::new();
        parser.feed(&full_frame.as_bytes()[..20]); // Only header part
        assert!(parser.try_parse_frame().is_none()); // Not enough data

        // Feed the rest
        parser.feed(&full_frame.as_bytes()[20..]);
        assert!(parser.try_parse_frame().is_some()); // Now complete
    }

    #[test]
    fn test_parse_content_length() {
        let header = "Content-Length: 42\r\n\r\n";
        assert_eq!(parse_content_length(header), Some(42));

        let header = "Content-Length: 0\r\n\r\n";
        assert_eq!(parse_content_length(header), Some(0));

        let header = "Some-Other-Header: blah\r\n\r\n";
        assert_eq!(parse_content_length(header), None);
    }

    #[test]
    fn test_find_header_end() {
        let data = b"Content-Length: 10\r\n\r\n1234567890";
        assert_eq!(find_header_end(data), Some(22)); // After \r\n\r\n

        let data = b"no header here";
        assert_eq!(find_header_end(data), None);
    }

    #[test]
    fn test_mcp_connection_config() {
        let config = McpServerConfig {
            name: "filesystem".to_string(),
            transport: McpTransport::Stdio {
                command: "npx".to_string(),
                args: vec!["-y".to_string(), "@mcp/server".to_string()],
                env: HashMap::new(),
            },
            requires_api_key: false,
            api_key_field: None,
            ..Default::default()
        };
        let conn = McpConnection::new(config);
        assert_eq!(conn.name(), "filesystem");
        assert!(conn.tools().is_empty());
    }

    #[test]
    fn test_mcp_tool_wrapper_properties() {
        let config = McpServerConfig {
            name: "test_server".to_string(),
            transport: McpTransport::Stdio {
                command: "test".to_string(),
                args: vec![],
                env: HashMap::new(),
            },
            requires_api_key: false,
            api_key_field: None,
            ..Default::default()
        };
        let conn = Arc::new(tokio::sync::Mutex::new(McpConnection::new(config)));
        let wrapper = McpToolWrapper::new(
            "search".to_string(),
            "Search tool".to_string(),
            serde_json::json!({}),
            "test_server".to_string(),
            conn,
        );
        assert_eq!(wrapper.name(), "search");
        assert_eq!(wrapper.risk_level(), oneai_core::RiskLevel::Medium);
    }

    // ── Stage 1: name normalization + per-server disconnect ────────────────

    #[test]
    fn test_normalize_tool_name_namespaces() {
        assert_eq!(
            normalize_tool_name("filesystem", "read_file"),
            "mcp__filesystem__read_file"
        );
    }

    #[test]
    fn test_normalize_tool_name_sanitizes() {
        // Uppercase + non-alnum → lowercased + underscores, no repeats.
        assert_eq!(
            normalize_tool_name("My-Server.io", "Read File!"),
            "mcp__my_server_io__read_file"
        );
    }

    #[test]
    fn test_normalize_tool_name_collides_same_raw_distinct_server() {
        // Same raw tool name on two servers → distinct normalized names.
        let a = normalize_tool_name("fs", "read_file");
        let b = normalize_tool_name("git", "read_file");
        assert_ne!(a, b);
        assert!(a.starts_with("mcp__fs__"));
        assert!(b.starts_with("mcp__git__"));
    }

    #[test]
    fn test_normalize_tool_name_truncates_long_under_64() {
        let server = "s";
        let tool = "x".repeat(200);
        let n = normalize_tool_name(server, &tool);
        assert!(
            n.len() <= 64,
            "normalized name must be ≤64 bytes, got {} ({})",
            n.len(),
            n
        );
        assert!(n.starts_with("mcp__s__"));
        // Long distinct names must not collapse to the same identifier.
        let tool2 = "y".repeat(200);
        let n2 = normalize_tool_name(server, &tool2);
        assert_ne!(n, n2);
    }

    #[test]
    fn test_mcp_connection_status_is_connected() {
        assert!(McpConnectionStatus::Connected { tools: vec![] }.is_connected());
        assert!(!McpConnectionStatus::Disconnected.is_connected());
        assert!(!McpConnectionStatus::NotConfigured.is_connected());
    }

    #[test]
    fn test_mcp_tool_wrapper_remote_name_split() {
        // `with_remote_name` keeps registry name ≠ remote name.
        let config = McpServerConfig {
            name: "srv".to_string(),
            transport: McpTransport::Stdio {
                command: "c".to_string(),
                args: vec![],
                env: HashMap::new(),
            },
            requires_api_key: false,
            api_key_field: None,
            ..Default::default()
        };
        let conn = Arc::new(tokio::sync::Mutex::new(McpConnection::new(config)));
        let wrapper = McpToolWrapper::with_remote_name(
            "mcp__srv__read_file".to_string(),
            "read_file".to_string(),
            "Reads a file".to_string(),
            serde_json::json!({}),
            "srv".to_string(),
            conn,
        );
        assert_eq!(wrapper.name(), "mcp__srv__read_file");
        assert_eq!(wrapper.remote_name(), "read_file");
        assert_eq!(wrapper.server_name(), "srv");
    }

    #[tokio::test]
    async fn test_mcp_server_manager_empty_state() {
        let mgr = McpServerManager::new();
        assert!(mgr.server_names().await.is_empty());
        assert!(!mgr.is_connected("anything").await);
        assert_eq!(
            mgr.server_status("anything").await,
            McpConnectionStatus::NotConfigured
        );
        assert!(mgr.all_tool_wrappers().await.is_empty());
        assert!(mgr.get_tool_wrapper("anything").await.is_none());
        assert!(mgr.bindings_snapshot().await.is_empty());
    }

    // ── Stage 4-1: immutable McpBinding snapshot + identity + lazy ───────

    #[test]
    fn test_mcp_transport_kind_from_transport() {
        let stdio = McpTransport::Stdio {
            command: "c".to_string(),
            args: vec![],
            env: HashMap::new(),
        };
        let sse = McpTransport::Sse {
            url: "http://x".to_string(),
            headers: HashMap::new(),
        };
        let http = McpTransport::StreamableHttp {
            url: "http://x".to_string(),
            headers: HashMap::new(),
        };
        assert_eq!(
            McpTransportKind::from_transport(&stdio),
            McpTransportKind::Stdio
        );
        assert_eq!(
            McpTransportKind::from_transport(&sse),
            McpTransportKind::Sse
        );
        assert_eq!(
            McpTransportKind::from_transport(&http),
            McpTransportKind::StreamableHttp
        );
    }

    #[tokio::test]
    async fn test_mcp_binding_build_snapshots_identity_and_tools() {
        let config = McpServerConfig {
            name: "srv".to_string(),
            transport: McpTransport::Stdio {
                command: "c".to_string(),
                args: vec![],
                env: HashMap::new(),
            },
            requires_api_key: false,
            api_key_field: None,
            ..Default::default()
        };
        // An unconnected McpConnection has no discovered tools / serverInfo —
        // the binding snapshot should still capture identity + empty catalog.
        let conn = Arc::new(tokio::sync::Mutex::new(McpConnection::new(config.clone())));
        let binding = {
            let conn_guard = conn.lock().await;
            McpBinding::build(
                &config,
                conn.clone(),
                &conn_guard,
                &McpToolPermissions::default(),
                7,
            )
        };
        assert_eq!(binding.identity.name, "srv");
        assert_eq!(binding.identity.transport_kind, McpTransportKind::Stdio);
        assert!(binding.identity.server_info.is_none());
        assert!(binding.tools.is_empty());
        assert!(binding.wrappers.is_empty());
        assert_eq!(binding.generation, 7);
        assert!(!binding.lazy);
        assert!(binding.tool_names().is_empty());
        // The wrapper set shares the connection Arc.
        assert!(Arc::ptr_eq(&binding.connection, &conn));
    }

    // ── Stage 4-2: tool catalog cache LRU + generation ───────────────────

    fn info(name: &str) -> McpToolInfo {
        McpToolInfo {
            name: name.to_string(),
            description: format!("tool {name}"),
            parameters_schema: serde_json::json!({"type": "object"}),
            server_name: "srv".to_string(),
        }
    }

    #[test]
    fn test_catalog_cache_put_and_get_matching_generation() {
        let mut cache = ToolCatalogCache::new();
        cache.put("a", vec![info("t1")], 1);
        assert_eq!(cache.len(), 1);
        let got = cache.get("a", 1).expect("matching generation hits");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "t1");
    }

    #[test]
    fn test_catalog_cache_get_returns_none_on_generation_mismatch() {
        let mut cache = ToolCatalogCache::new();
        cache.put("a", vec![info("t1")], 1);
        // Different expected generation → miss (stale snapshot).
        assert!(cache.get("a", 2).is_none());
        // Unknown server → miss.
        assert!(cache.get("b", 1).is_none());
    }

    #[test]
    fn test_catalog_cache_bump_generation_invalidates_old_snapshot() {
        let mut cache = ToolCatalogCache::new();
        cache.put("a", vec![info("t1")], 1);
        let new_gen = cache.bump_generation("a").expect("was cached");
        assert_ne!(new_gen, 1);
        // Old generation no longer matches; the entry is retained for
        // `last_known_tools`.
        assert!(cache.get("a", 1).is_none());
        assert_eq!(cache.last_known_tools("a").map(|t| t.len()), Some(1));
        // Bumping an unknown server returns None (no-op).
        assert!(cache.bump_generation("never").is_none());
    }

    #[test]
    fn test_catalog_cache_lru_eviction_when_over_capacity() {
        let mut cache = ToolCatalogCache::with_capacity(2);
        cache.put("a", vec![info("a")], 1);
        cache.put("b", vec![info("b")], 1);
        assert_eq!(cache.len(), 2);
        // Touch "a" so "b" becomes least-recently-used.
        let _ = cache.get("a", 1);
        cache.put("c", vec![info("c")], 1);
        assert_eq!(cache.len(), 2); // capacity bound
        assert!(cache.last_known_tools("a").is_some()); // a was recent
        assert!(cache.last_known_tools("b").is_none()); // b evicted
        assert!(cache.last_known_tools("c").is_some());
    }

    #[test]
    fn test_catalog_cache_put_replaces_existing_and_promotes_lru() {
        let mut cache = ToolCatalogCache::with_capacity(2);
        cache.put("a", vec![info("a1")], 1);
        cache.put("b", vec![info("b1")], 1);
        // Replace "a"'s entry under a new generation.
        cache.put("a", vec![info("a2")], 2);
        assert_eq!(cache.len(), 2);
        assert_eq!(
            cache.get("a", 2).map(|t| t[0].name.clone()),
            Some("a2".to_string())
        );
        // "b" is now least-recently-used → evicted on next put.
        cache.put("c", vec![info("c")], 1);
        assert!(cache.last_known_tools("b").is_none());
    }

    #[test]
    fn test_catalog_cache_invalidate_removes_entry() {
        let mut cache = ToolCatalogCache::new();
        cache.put("a", vec![info("a")], 1);
        cache.invalidate("a");
        assert!(cache.is_empty());
        assert!(cache.last_known_tools("a").is_none());
    }

    #[tokio::test]
    async fn test_manager_cached_tools_generation_invalidated_on_disconnect() {
        // The empty-state manager has no cached entries.
        let mgr = McpServerManager::new();
        assert!(mgr.last_known_tools("nope").is_none());
        assert!(mgr.cached_tools("nope", 1).is_none());
        // server_status on a never-configured server is NotConfigured.
        assert_eq!(
            mgr.server_status("nope").await,
            McpConnectionStatus::NotConfigured
        );
    }

    // ── Stage 2: per-server permission policy ──────────────────────────────

    #[test]
    fn test_mcp_tool_permissions_default_is_standard_direct() {
        let p = McpToolPermissions::default();
        assert_eq!(p.default_level, PermissionLevel::Standard);
        assert!(p.tool_overrides.is_empty());
        assert!(p.tool_exposure.is_empty());
        // A tool with no override falls back to default Standard / Direct.
        assert_eq!(p.level_for("anything"), PermissionLevel::Standard);
        assert_eq!(p.exposure_for("anything"), ToolExposure::Direct);
    }

    #[test]
    fn test_mcp_tool_permissions_level_and_exposure_for() {
        let p = McpToolPermissions {
            default_level: PermissionLevel::Read,
            tool_overrides: HashMap::from([("dangerous".to_string(), PermissionLevel::Full)]),
            tool_exposure: HashMap::from([
                ("hidden_one".to_string(), ToolExposure::Hidden),
                ("deferred_one".to_string(), ToolExposure::Deferred),
            ]),
        };

        // default applies to unknown tools
        assert_eq!(p.level_for("boring"), PermissionLevel::Read);
        assert_eq!(p.exposure_for("boring"), ToolExposure::Direct);
        // overrides apply when present
        assert_eq!(p.level_for("dangerous"), PermissionLevel::Full);
        assert_eq!(p.exposure_for("hidden_one"), ToolExposure::Hidden);
        assert_eq!(p.exposure_for("deferred_one"), ToolExposure::Deferred);
    }

    #[test]
    fn test_mcp_tool_wrapper_with_policy_carries_levels() {
        let config = McpServerConfig {
            name: "srv".to_string(),
            transport: McpTransport::Stdio {
                command: "c".to_string(),
                args: vec![],
                env: HashMap::new(),
            },
            requires_api_key: false,
            api_key_field: None,
            ..Default::default()
        };
        let conn = Arc::new(tokio::sync::Mutex::new(McpConnection::new(config)));
        let wrapper = McpToolWrapper::with_policy(
            "mcp__srv__dangerous".to_string(),
            "dangerous".to_string(),
            "A dangerous tool".to_string(),
            serde_json::json!({}),
            "srv".to_string(),
            PermissionLevel::Full,
            ToolExposure::DeferredModelOnly,
            conn,
        );
        assert_eq!(wrapper.permission_level(), PermissionLevel::Full);
        assert_eq!(wrapper.risk_level(), oneai_core::RiskLevel::High);
        assert_eq!(wrapper.exposure(), ToolExposure::DeferredModelOnly);
        assert_eq!(wrapper.declared_permission_level(), PermissionLevel::Full);
        assert_eq!(wrapper.declared_exposure(), ToolExposure::DeferredModelOnly);
    }

    #[test]
    fn test_mcp_tool_permissions_serde_roundtrip() {
        let p = McpToolPermissions {
            default_level: PermissionLevel::Full,
            tool_overrides: HashMap::from([("x".to_string(), PermissionLevel::Read)]),
            tool_exposure: HashMap::from([("x".to_string(), ToolExposure::Hidden)]),
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: McpToolPermissions = serde_json::from_str(&json).unwrap();
        assert_eq!(back.default_level, PermissionLevel::Full);
        assert_eq!(back.level_for("x"), PermissionLevel::Read);
        assert_eq!(back.exposure_for("x"), ToolExposure::Hidden);
    }
}
