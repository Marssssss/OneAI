//! End-to-end OAuth 401 → refresh → retry test for a StreamableHttp MCP server.
//!
//! A mock HTTP server plays both roles:
//! - `POST /mcp` — the MCP JSON-RPC endpoint. `initialize` + `tools/list`
//!   succeed with either bearer (`valid` or `rotated`); `tools/call` returns
//!   401 for the stale `valid` bearer and 200 for `rotated`.
//! - `POST /token` — the OAuth token endpoint; a refresh returns
//!   `access_token=rotated`.
//!
//! The registry is configured with an OAuth entry, the store is pre-seeded
//! with a `valid` access token, and a tool call is driven through the wrapper.
//! The connection's 401 path must trigger a refresh, persist the new token,
//! and retry the call once — surfacing the tool's success.

use std::sync::Arc;

use chrono::{Duration, Utc};
use oneai_mcp::{
    McpOAuthConfig, McpPluginEntry, McpPluginRegistry, McpPluginSource, OAuthStoredTokens,
    OAuthTokenStore, Pkce,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Spawn the combined mock MCP + OAuth token server. Returns
/// `(mcp_url, token_url, join_handle)`.
async fn spawn_server() -> (String, String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mcp_url = format!("http://127.0.0.1:{}/mcp", port);
    let token_url = format!("http://127.0.0.1:{}/token", port);
    let handle = tokio::spawn(async move {
        for _ in 0..32 {
            let (mut sock, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => break,
            };
            let req = read_request(&mut sock).await;
            let first = req.lines().next().unwrap_or("");

            let (status, body, session_id) = if first.contains("/token") {
                // OAuth refresh → rotated tokens.
                (
                    200,
                    r#"{"access_token":"rotated","token_type":"Bearer","refresh_token":"rt2","expires_in":3600,"scope":"mcp"}"#
                        .to_string(),
                    false,
                )
            } else if first.contains("/mcp") {
                let bearer = extract_bearer(&req);
                let method = extract_method(&req);
                // Only `tools/call` with the stale `valid` bearer 401s — the
                // refresh+retry path must recover. Everything else 200s.
                let (s, b) = if method == "tools/call" && bearer == Some("valid") {
                    (401, String::new())
                } else if method == "initialize" {
                    (200, r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"mock","version":"1"}}}"#.to_string())
                } else if method == "tools/list" {
                    (200, r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"ping","description":"ping","inputSchema":{"type":"object","properties":{}}}]}}"#.to_string())
                } else if method == "tools/call" && bearer == Some("rotated") {
                    (200, r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"pong"}]}}"#.to_string())
                } else if method == "notifications/initialized" {
                    (200, "{}".to_string())
                } else {
                    (401, String::new())
                };
                (s, b, method == "initialize")
            } else {
                (200, "{}".to_string(), false)
            };

            if status == 401 {
                let resp = "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"mcp\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
                continue;
            }
            let session_header = if session_id {
                "Mcp-Session-Id: sess-mock\r\n"
            } else {
                ""
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                session_header,
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
        }
    });
    (mcp_url, token_url, handle)
}

fn extract_bearer(req: &str) -> Option<&str> {
    for line in req.lines() {
        if line.to_ascii_lowercase().starts_with("authorization:") {
            let v = line.split_once(':')?.1.trim();
            return v.strip_prefix("Bearer ").map(str::trim);
        }
    }
    None
}

fn extract_method(req: &str) -> String {
    // Find the JSON body (after the blank line), parse `"method"`.
    let body = req.split("\r\n\r\n").nth(1).unwrap_or("");
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(m) = v.get("method").and_then(|m| m.as_str()) {
            return m.to_string();
        }
    }
    String::new()
}

/// Read a full HTTP request (headers + Content-Length body).
async fn read_request<S: AsyncReadExt + Unpin>(stream: &mut S) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(hend) = find_subslice(&buf, b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&buf[..hend]).to_string();
            let cl = headers
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    if k.trim().eq_ignore_ascii_case("content-length") {
                        v.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            let body_start = hend + 4;
            if buf.len() >= body_start + cl {
                break;
            }
        }
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).to_string()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

#[tokio::test(flavor = "current_thread")]
async fn oauth_401_triggers_refresh_and_retry() {
    let (mcp_url, token_url, _handle) = spawn_server().await;

    let tmp = tempfile::TempDir::new().unwrap();
    let store = OAuthTokenStore::new(tmp.path().join("oauth").to_path_buf());
    // Pre-seed a "valid" (not expired) token whose refresh yields "rotated".
    let seed = OAuthStoredTokens {
        access_token: "valid".to_string(),
        token_type: "Bearer".to_string(),
        refresh_token: Some("rt1".to_string()),
        expires_at: Some(Utc::now() + Duration::seconds(3600)),
        scope: Some("mcp".to_string()),
        token_endpoint: token_url.clone(),
        client_id: "cid".to_string(),
        client_secret: None,
        scopes: vec!["mcp".to_string()],
    };
    store.save("remote", &seed).unwrap();

    // Build the registry with the temp store + an OAuth streamable_http entry.
    let mut registry = McpPluginRegistry::with_token_store(store.clone());
    let entry = McpPluginEntry {
        name: "remote".to_string(),
        description: "mock oauth MCP".to_string(),
        source: McpPluginSource::StreamableHttp {
            url: mcp_url,
            headers: Default::default(),
        },
        enabled: true,
        requires_api_key: false,
        api_key_env: None,
        tags: vec![],
        permissions: Default::default(),
        oauth: Some(McpOAuthConfig {
            client_id: Some("cid".to_string()),
            use_dynamic_registration: false,
            ..Default::default()
        }),
        ..Default::default()
    };
    registry.add_entry(entry);

    // Connect — the "valid" bearer is injected, initialize + tools/list succeed.
    let tools = registry.connect_server("remote").await.expect("connect");
    assert_eq!(tools.len(), 1);
    assert!(tools[0].ends_with("__ping"));

    // Register the wrapper so we can drive it like the agent loop would.
    let tool_registry = Arc::new(oneai_tool::ToolRegistry::new());
    registry.register_tools(&tool_registry).await.unwrap();
    let wrapper = tool_registry
        .get("mcp__remote__ping")
        .await
        .expect("registered")
        .clone();

    // Drive a tool call. The stale `valid` bearer 401s; the connection must
    // refresh + retry once with `rotated` and return the tool's success.
    let out = wrapper
        .execute(serde_json::json!({}))
        .await
        .expect("tool call");
    assert!(
        out.success,
        "expected success after refresh+retry; error: {:?}",
        out.error
    );
    assert!(out.content.contains("pong"));

    // The store now holds the rotated token.
    let reloaded = store.load("remote").unwrap();
    assert_eq!(reloaded.access_token, "rotated");

    // PKCE / state are unused here but the module must still compile in the
    // integration test binary — reference them lightly.
    let _ = Pkce::generate();
}
