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
use oneai_core::{PermissionLevel, ToolOutput};
use oneai_core::error::Result;
use oneai_core::traits::Tool;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
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
        let json: serde_json::Value = serde_json::from_slice(body_bytes)
            .ok()?; // If JSON parsing fails, skip this frame

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
    fn default() -> Self { Self::new() }
}

/// Find the end of the HTTP-like header section (\r\n\r\n).
fn find_header_end(buffer: &[u8]) -> Option<usize> {
    for i in 0..buffer.len().saturating_sub(3) {
        if buffer[i] == b'\r' && buffer[i+1] == b'\n'
            && buffer[i+2] == b'\r' && buffer[i+3] == b'\n' {
            return Some(i + 4); // Include the final \r\n\r\n
        }
    }
    None
}

/// Parse the Content-Length value from an HTTP-like header.
fn parse_content_length(header: &str) -> Option<usize> {
    for line in header.lines() {
        if line.starts_with("Content-Length:") || line.starts_with("Content-Length: ") {
            let value = line.trim_start_matches("Content-Length:")
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

// ─── McpServerConfig ────────────────────────────────────────────────────────

/// Configuration for a MCP server connection.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
    pub requires_api_key: bool,
    pub api_key_field: Option<String>,
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

                let mut child = cmd.spawn()
                    .map_err(|e| oneai_core::error::OneAIError::Provider(
                        format!("Failed to launch MCP server '{}': {}", command, e)
                    ))?;

                let stdin = child.stdin.take()
                    .ok_or_else(|| oneai_core::error::OneAIError::Provider("No stdin pipe".to_string()))?;
                let stdout = child.stdout.take()
                    .ok_or_else(|| oneai_core::error::OneAIError::Provider("No stdout pipe".to_string()))?;

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
                    return Err(oneai_core::error::OneAIError::Provider(
                        format!("MCP initialize error: {}", error)
                    ));
                }

                tracing::info!("MCP initialized with server '{}' — capabilities: {}",
                    self.config.name,
                    init_response.get("result")
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
                            let name = tool_def.get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let description = tool_def.get("description")
                                .and_then(|d| d.as_str())
                                .unwrap_or("")
                                .to_string();
                            let schema = tool_def.get("inputSchema")
                                .cloned()
                                .unwrap_or(serde_json::json!({"type": "object"}));

                            self.tools.insert(name.clone(), McpToolInfo {
                                name,
                                description,
                                parameters_schema: schema,
                                server_name: self.config.name.clone(),
                            });
                        }
                    }
                }

                tracing::info!("MCP connection established with server '{}' via Stdio — discovered {} tools",
                    self.config.name, self.tools.len());

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
                let client = Self::build_http_client(&headers)?;

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
                let init_response = Self::http_post_json(&client, url, &headers, &init_request).await?;

                // Verify initialize response
                if init_response.get("error").is_some() {
                    let error = init_response.get("error").unwrap();
                    return Err(oneai_core::error::OneAIError::Provider(
                        format!("MCP SSE initialize error: {}", error)
                    ));
                }

                tracing::info!("MCP initialized with SSE server '{}' — capabilities: {}",
                    self.config.name,
                    init_response.get("result")
                        .and_then(|r| r.get("capabilities"))
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                );

                // Step 2: Send initialized notification
                let initialized_notification = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                });
                Self::http_post_json(&client, url, &headers, &initialized_notification).await?;

                // Step 3: Send tools/list request
                let list_tools_request = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": self.next_id,
                    "method": "tools/list",
                    "params": {}
                });
                self.next_id += 1;

                let tools_response = Self::http_post_json(&client, url, &headers, &list_tools_request).await?;

                // Parse tool definitions
                if let Some(result) = tools_response.get("result") {
                    if let Some(tool_list) = result.get("tools").and_then(|t| t.as_array()) {
                        for tool_def in tool_list {
                            let name = tool_def.get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let description = tool_def.get("description")
                                .and_then(|d| d.as_str())
                                .unwrap_or("")
                                .to_string();
                            let schema = tool_def.get("inputSchema")
                                .cloned()
                                .unwrap_or(serde_json::json!({"type": "object"}));

                            self.tools.insert(name.clone(), McpToolInfo {
                                name,
                                description,
                                parameters_schema: schema,
                                server_name: self.config.name.clone(),
                            });
                        }
                    }
                }

                // Store the HTTP client and URLs for future calls
                self.http_client = Some(client);
                self.sse_url = Some(url.clone());
                self.post_url = Some(url.clone());

                tracing::info!("MCP SSE connection established with server '{}' — discovered {} tools",
                    self.config.name, self.tools.len());

                Ok(())
            }
            McpTransport::StreamableHttp { url, headers } => {
                // StreamableHttp transport: POST requests, SSE stream responses
                // Similar to SSE but with session management.
                // The server returns a session ID in the initial response
                // that must be included in subsequent requests.
                let client = Self::build_http_client(&headers)?;

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

                let (init_response, resp_headers) = Self::http_post_with_headers(&client, url, &headers, &init_request).await?;

                // Extract session ID from response headers (if provided)
                // MCP StreamableHttp uses Mcp-Session-Id header
                let session_id = resp_headers.get("mcp-session-id")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());

                if init_response.get("error").is_some() {
                    let error = init_response.get("error").unwrap();
                    return Err(oneai_core::error::OneAIError::Provider(
                        format!("MCP StreamableHttp initialize error: {}", error)
                    ));
                }

                tracing::info!("MCP initialized with StreamableHttp server '{}' — session_id: {:?}",
                    self.config.name, session_id);

                // Step 2: Send initialized notification
                let initialized_notification = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                });
                Self::http_post_with_session(&client, url, &headers, session_id.as_deref(), &initialized_notification).await?;

                // Step 3: Send tools/list request
                let list_tools_request = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": self.next_id,
                    "method": "tools/list",
                    "params": {}
                });
                self.next_id += 1;

                let (tools_response, _) = Self::http_post_with_session(&client, url, &headers, session_id.as_deref(), &list_tools_request).await?;

                // Parse tool definitions
                if let Some(result) = tools_response.get("result") {
                    if let Some(tool_list) = result.get("tools").and_then(|t| t.as_array()) {
                        for tool_def in tool_list {
                            let name = tool_def.get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let description = tool_def.get("description")
                                .and_then(|d| d.as_str())
                                .unwrap_or("")
                                .to_string();
                            let schema = tool_def.get("inputSchema")
                                .cloned()
                                .unwrap_or(serde_json::json!({"type": "object"}));

                            self.tools.insert(name.clone(), McpToolInfo {
                                name,
                                description,
                                parameters_schema: schema,
                                server_name: self.config.name.clone(),
                            });
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
    pub async fn call_tool(
        &mut self,
        tool_name: &str,
        args: serde_json::Value,
    ) -> Result<ToolOutput> {
        match &self.config.transport {
            McpTransport::Stdio { .. } => {
                // Use persistent connection (no re-spawn!)
                if self.stdin_writer.is_none() || self.stdout_reader.is_none() {
                    return Ok(ToolOutput {
                        success: false,
                        content: String::new(),
                        error: Some("MCP connection not established — call connect_and_discover() first".to_string()),
                    });
                }

                // Send tools/call request
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

                self.send_jsonrpc(&call_request).await?;
                let call_response = self.read_jsonrpc_response().await?;

                Self::parse_tool_call_response(&call_response)
            }
            McpTransport::Sse { url, headers } => {
                // Use HTTP POST for SSE transport
                let client = self.http_client.as_ref()
                    .ok_or_else(|| oneai_core::error::OneAIError::Provider(
                        "SSE HTTP client not initialized".to_string()
                    ))?;

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

                let response = Self::http_post_json(client, url, headers, &call_request).await?;
                Self::parse_tool_call_response(&response)
            }
            McpTransport::StreamableHttp { url, headers } => {
                // Use HTTP POST with session ID for StreamableHttp transport
                let client = self.http_client.as_ref()
                    .ok_or_else(|| oneai_core::error::OneAIError::Provider(
                        "StreamableHttp HTTP client not initialized".to_string()
                    ))?;

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

                let (response, _) = Self::http_post_with_session(
                    client, url, headers,
                    self.session_id.as_deref(),
                    &call_request,
                ).await?;
                Self::parse_tool_call_response(&response)
            }
        }
    }

    /// Send a JSON-RPC message via the persistent stdin connection.
    async fn send_jsonrpc(&mut self, message: &serde_json::Value) -> Result<()> {
        if let Some(writer) = &mut self.stdin_writer {
            let json_str = serde_json::to_string(message)
                .map_err(|e| oneai_core::error::OneAIError::Provider(
                    format!("JSON serialization error: {}", e)
                ))?;

            let frame = format!("Content-Length: {}\r\n\r\n{}", json_str.len(), json_str);
            writer.write_all(frame.as_bytes()).await
                .map_err(|e| oneai_core::error::OneAIError::Provider(
                    format!("MCP write error: {}", e)
                ))?;
            writer.flush().await
                .map_err(|e| oneai_core::error::OneAIError::Provider(
                    format!("MCP flush error: {}", e)
                ))?;
            Ok(())
        } else {
            Err(oneai_core::error::OneAIError::Provider("No MCP stdin connection".to_string()))
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
                let n = reader.read(&mut buffer).await
                    .map_err(|e| oneai_core::error::OneAIError::Provider(
                        format!("MCP read error: {}", e)
                    ))?;

                if n == 0 {
                    // EOF — subprocess has closed stdout
                    return Err(oneai_core::error::OneAIError::Provider(
                        "MCP server closed stdout (process may have exited)".to_string()
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
            Err(oneai_core::error::OneAIError::Provider("No MCP stdout connection".to_string()))
        }
    }

    /// Shutdown the MCP connection — kill subprocess or close HTTP client.
    pub async fn shutdown(&mut self) -> Result<()> {
        if let Some(child) = &mut self.child {
            // Try graceful shutdown first (SIGTERM on Unix)
            child.kill().await
                .map_err(|e| oneai_core::error::OneAIError::Provider(
                    format!("Failed to kill MCP subprocess: {}", e)
                ))?;
            tracing::info!("MCP connection to server '{}' shut down (Stdio)", self.config.name);
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

    // ─── HTTP Helper Methods ─────────────────────────────────────────────────

    /// Build an HTTP client with optional custom headers.
    fn build_http_client(headers: &HashMap<String, String>) -> Result<reqwest::Client> {
        let mut builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30));

        // Add custom headers as default request headers
        for (key, value) in headers {
            builder = builder.default_headers(
                reqwest::header::HeaderMap::from_iter(
                    std::iter::once((
                        reqwest::header::HeaderName::from_bytes(key.as_bytes())
                            .unwrap_or_else(|_| reqwest::header::AUTHORIZATION),
                        reqwest::header::HeaderValue::from_str(value)
                            .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static(""))
                    ))
                )
            );
        }

        builder.build()
            .map_err(|e| oneai_core::error::OneAIError::Provider(
                format!("Failed to build HTTP client: {}", e)
            ))
    }

    /// Send a JSON-RPC message via HTTP POST and receive the JSON response.
    async fn http_post_json(
        client: &reqwest::Client,
        url: &str,
        headers: &HashMap<String, String>,
        message: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let (response, _) = Self::http_post_with_headers(client, url, headers, message).await?;
        Ok(response)
    }

    /// Send a JSON-RPC message via HTTP POST and receive response + headers.
    async fn http_post_with_headers(
        client: &reqwest::Client,
        url: &str,
        headers: &HashMap<String, String>,
        message: &serde_json::Value,
    ) -> Result<(serde_json::Value, reqwest::header::HeaderMap)> {
        let mut request = client.post(url)
            .json(message);

        // Add any custom headers to this specific request
        for (key, value) in headers {
            if let Ok(header_name) = reqwest::header::HeaderName::from_bytes(key.as_bytes()) {
                if let Ok(header_value) = reqwest::header::HeaderValue::from_str(value) {
                    request = request.header(header_name, header_value);
                }
            }
        }

        let response = request.send().await
            .map_err(|e| oneai_core::error::OneAIError::Provider(
                format!("MCP HTTP POST error to {}: {}", url, e)
            ))?;

        let resp_headers = response.headers().clone();

        // Check status code
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(oneai_core::error::OneAIError::Provider(
                format!("MCP HTTP POST returned status {} from {}: {}", status.as_u16(), url, body)
            ));
        }

        // Parse response body as JSON
        let body = response.text().await
            .map_err(|e| oneai_core::error::OneAIError::Provider(
                format!("MCP HTTP POST read error: {}", e)
            ))?;

        // The response might be SSE format (multiple events) or plain JSON
        // Try parsing as plain JSON first
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
            return Ok((json, resp_headers));
        }

        // Try parsing as SSE format (event: message, data: {...})
        // Find the JSON data in SSE events
        for line in body.lines() {
            if line.starts_with("data: ") {
                let data = line.trim_start_matches("data: ").trim();
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                    return Ok((json, resp_headers));
                }
            }
        }

        // If neither works, return an error
        Err(oneai_core::error::OneAIError::Provider(
            format!("MCP HTTP POST response could not be parsed as JSON from {}: {}", url, body)
        ))
    }

    /// Send a JSON-RPC message via HTTP POST with session ID header.
    async fn http_post_with_session(
        client: &reqwest::Client,
        url: &str,
        headers: &HashMap<String, String>,
        session_id: Option<&str>,
        message: &serde_json::Value,
    ) -> Result<(serde_json::Value, reqwest::header::HeaderMap)> {
        let mut request = client.post(url)
            .json(message);

        // Add session ID header if present
        if let Some(sid) = session_id {
            request = request.header("Mcp-Session-Id", sid);
        }

        // Add custom headers
        for (key, value) in headers {
            if let Ok(header_name) = reqwest::header::HeaderName::from_bytes(key.as_bytes()) {
                if let Ok(header_value) = reqwest::header::HeaderValue::from_str(value) {
                    request = request.header(header_name, header_value);
                }
            }
        }

        let response = request.send().await
            .map_err(|e| oneai_core::error::OneAIError::Provider(
                format!("MCP HTTP POST error to {}: {}", url, e)
            ))?;

        let resp_headers = response.headers().clone();

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(oneai_core::error::OneAIError::Provider(
                format!("MCP HTTP POST returned status {} from {}: {}", status.as_u16(), url, body)
            ));
        }

        let body = response.text().await
            .map_err(|e| oneai_core::error::OneAIError::Provider(
                format!("MCP HTTP POST read error: {}", e)
            ))?;

        // Parse response (plain JSON or SSE format)
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

        Err(oneai_core::error::OneAIError::Provider(
            format!("MCP HTTP POST response could not be parsed from {}: {}", url, &body[..body.len().min(200)])
        ))
    }

    /// Parse a tool call response from any transport into a ToolOutput.
    fn parse_tool_call_response(response: &serde_json::Value) -> Result<ToolOutput> {
        // Check for errors
        if let Some(error) = response.get("error") {
            let error_msg = error.get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("Unknown MCP error");
            return Ok(ToolOutput {
                success: false,
                content: String::new(),
                error: Some(format!("MCP tool error: {}", error_msg)),
            });
        }

        // Extract content from the result
        let content = if let Some(result) = response.get("result") {
            if let Some(content_arr) = result.get("content").and_then(|c| c.as_array()) {
                // MCP returns content as an array of content blocks
                content_arr.iter()
                    .filter_map(|block| {
                        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                            block.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
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
    name: String,
    description: String,
    parameters_schema: serde_json::Value,
    server_name: String,
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
        Self { name, description, parameters_schema, server_name, connection }
    }
}

#[async_trait]
impl Tool for McpToolWrapper {
    fn name(&self) -> &str { &self.name }
    fn description(&self) -> &str { &self.description }
    fn parameters_schema(&self) -> serde_json::Value { self.parameters_schema.clone() }

    fn risk_level(&self) -> oneai_core::RiskLevel {
        self.permission_level().to_risk_level()
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let mut conn = self.connection.lock().await;
        conn.call_tool(&self.name, args).await
    }
}

impl PermissionAwareTool for McpToolWrapper {
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Standard
    }
}

use crate::tool_interfaces::PermissionAwareTool;

// ─── McpServerManager ────────────────────────────────────────────────────────

/// MCP server manager — handles real connections to MCP servers.
pub struct McpServerManager {
    connections: HashMap<String, Arc<tokio::sync::Mutex<McpConnection>>>,
    tool_wrappers: HashMap<String, Arc<McpToolWrapper>>,
}

impl McpServerManager {
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
            tool_wrappers: HashMap::new(),
        }
    }

    /// Connect to an MCP server, discover tools, and create wrappers.
    pub async fn connect_server(&mut self, config: McpServerConfig) -> Result<Vec<String>> {
        let mut connection = McpConnection::new(config.clone());
        connection.connect_and_discover().await?;

        let connection_arc = Arc::new(tokio::sync::Mutex::new(connection));
        self.connections.insert(config.name.clone(), connection_arc.clone());

        let mut tool_names = Vec::new();
        // We need to read tools from the connection (it's locked in the Arc<Mutex>)
        let conn = connection_arc.lock().await;
        for (tool_name, tool_info) in conn.tools() {
            let wrapper = McpToolWrapper::new(
                tool_info.name.clone(),
                tool_info.description.clone(),
                tool_info.parameters_schema.clone(),
                tool_info.server_name.clone(),
                connection_arc.clone(),
            );
            tool_names.push(tool_name.clone());
            self.tool_wrappers.insert(tool_name.clone(), Arc::new(wrapper));
        }

        Ok(tool_names)
    }

    /// Get all tool wrappers.
    pub fn all_tool_wrappers(&self) -> Vec<Arc<McpToolWrapper>> {
        self.tool_wrappers.values().cloned().collect()
    }

    /// Get a tool wrapper by name.
    pub fn get_tool_wrapper(&self, name: &str) -> Option<&Arc<McpToolWrapper>> {
        self.tool_wrappers.get(name)
    }

    /// Shutdown all MCP connections.
    pub async fn shutdown_all(&mut self) -> Result<()> {
        for (_name, conn_arc) in &self.connections {
            let mut conn = conn_arc.lock().await;
            conn.shutdown().await?;
        }
        self.connections.clear();
        self.tool_wrappers.clear();
        Ok(())
    }
}

impl Default for McpServerManager {
    fn default() -> Self { Self::new() }
}

// ─── Pre-registered default MCP servers ──────────────────────────────────────

pub fn default_mcp_configs() -> Vec<McpServerConfig> {
    vec![
        McpServerConfig {
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
        },
    ]
}

pub fn optional_mcp_configs() -> Vec<McpServerConfig> {
    vec![
        McpServerConfig {
            name: "web_search".to_string(),
            transport: McpTransport::Stdio {
                command: "npx".to_string(),
                args: vec![
                    "-y".to_string(),
                    "@anthropic-ai/mcp-web-search".to_string(),
                ],
                env: HashMap::new(),
            },
            requires_api_key: true,
            api_key_field: Some("ANTHROPIC_API_KEY".to_string()),
        },
    ]
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
            str1.len(), str1, str2.len(), str2
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
}
