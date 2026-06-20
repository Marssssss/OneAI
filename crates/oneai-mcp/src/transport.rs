//! MCP Transport — Stdio transport for MCP server hosting.
//!
//! Implements the MCP Content-Length framing protocol for reading from stdin
//! and writing to stdout. This is the primary transport mode for MCP servers
//! that are launched as subprocesses by MCP clients (Claude Code, Cursor, etc.).

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

use crate::router::McpRouter;

/// MCP Stdio transport — reads JSON-RPC from stdin, writes responses to stdout.
///
/// Uses the Content-Length framing protocol:
/// ```text
/// Content-Length: 123\r\n
/// \r\n
/// <123 bytes of JSON body>
/// ```
///
/// The transport runs an event loop:
/// 1. Read a frame from stdin (Content-Length header + JSON body)
/// 2. Parse the JSON-RPC message
/// 3. Dispatch to the router
/// 4. Write the response frame to stdout
///
/// This is the standard transport mode for MCP servers launched by clients.
pub struct McpStdioTransport {
    /// Router for dispatching messages to handlers.
    router: Arc<McpRouter>,
}

impl McpStdioTransport {
    /// Create a new Stdio transport with the given router.
    pub fn new(router: Arc<McpRouter>) -> Self {
        Self { router }
    }

    /// Run the Stdio transport event loop.
    ///
    /// Reads JSON-RPC messages from stdin, dispatches them via the router,
    /// and writes responses to stdout. Runs until stdin is closed or an
    /// irrecoverable error occurs.
    pub async fn run(&self) -> oneai_core::error::Result<()> {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        let mut reader = BufReader::new(stdin);
        let mut writer = tokio::io::BufWriter::new(stdout);

        let mut framing_parser = McpFramingParser::new();
        let mut buffer = [0u8; 8192];

        tracing::info!("MCP Stdio transport started — awaiting messages from stdin");

        loop {
            // Read data from stdin
            let n = reader.read(&mut buffer).await
                .map_err(|e| oneai_core::error::OneAIError::Provider(
                    format!("MCP stdin read error: {}", e)
                ))?;

            if n == 0 {
                // EOF — stdin closed, client has disconnected
                tracing::info!("MCP Stdio transport — stdin closed, shutting down");
                return Ok(());
            }

            // Feed data to framing parser
            framing_parser.feed(&buffer[..n]);

            // Process all complete frames
            let frames = framing_parser.parse_all_frames();
            for frame in frames {
                // Dispatch the message and get a response
                let response = self.router.dispatch(frame).await;

                // Write the response frame to stdout
                self.write_frame(&mut writer, &response).await?;
            }
        }
    }

    /// Write a JSON-RPC response frame to stdout.
    ///
    /// Uses the Content-Length framing protocol:
    /// ```text
    /// Content-Length: <len>\r\n
    /// \r\n
    /// <JSON body>
    /// ```
    async fn write_frame(
        &self,
        writer: &mut tokio::io::BufWriter<tokio::io::Stdout>,
        message: &serde_json::Value,
    ) -> oneai_core::error::Result<()> {
        let json_str = serde_json::to_string(message)
            .map_err(|e| oneai_core::error::OneAIError::Provider(
                format!("JSON serialization error: {}", e)
            ))?;

        let frame = format!("Content-Length: {}\r\n\r\n{}", json_str.len(), json_str);
        writer.write_all(frame.as_bytes()).await
            .map_err(|e| oneai_core::error::OneAIError::Provider(
                format!("MCP stdout write error: {}", e)
            ))?;
        writer.flush().await
            .map_err(|e| oneai_core::error::OneAIError::Provider(
                format!("MCP stdout flush error: {}", e)
            ))?;

        Ok(())
    }
}

// ─── McpFramingParser ────────────────────────────────────────────────────────

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
        let json: serde_json::Value = serde_json::from_slice(body_bytes).ok()?;

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

        let mut parser = McpFramingParser::new();
        parser.feed(&full_frame.as_bytes()[..20]);
        assert!(parser.try_parse_frame().is_none());

        parser.feed(&full_frame.as_bytes()[20..]);
        assert!(parser.try_parse_frame().is_some());
    }

    #[test]
    fn test_parse_content_length() {
        assert_eq!(parse_content_length("Content-Length: 42\r\n\r\n"), Some(42));
        assert_eq!(parse_content_length("Content-Length: 0\r\n\r\n"), Some(0));
        assert_eq!(parse_content_length("Some-Other-Header: blah\r\n\r\n"), None);
    }

    #[test]
    fn test_find_header_end() {
        assert_eq!(find_header_end(b"Content-Length: 10\r\n\r\n1234567890"), Some(22));
        assert_eq!(find_header_end(b"no header here"), None);
    }

    #[tokio::test]
    async fn test_stdio_transport_creation() {
        let registry = Arc::new(oneai_tool::ToolRegistry::new());
        let handler = Arc::new(crate::handler::McpHandler::new(
            registry,
            crate::server::McpServerInfo::default(),
        ));
        let router = Arc::new(crate::router::McpRouter::new(handler));
        let _transport = McpStdioTransport::new(router);
        // Just verify creation — actual run requires stdin/stdout
        assert!(true);
    }
}
