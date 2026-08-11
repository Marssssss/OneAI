//! MCP OAuth 2.0 — PKCE authorization-code flow for HTTP-transport MCP servers.
//!
//! Implements the full OAuth leg the codex MCP client documents:
//! **discovery → (dynamic) registration → authorize → exchange → store →
//! refresh → retry**. Two login UXes are supported, switchable per invocation:
//!
//! - **Loopback redirect** (default) — a one-shot `TcpListener` on
//!   `127.0.0.1:{port}` is the `redirect_uri`; the authorize URL is printed
//!   and a best-effort `open`/`xdg-open`/`start` launches the system browser.
//!   The authorization server redirects back with `?code=…&state=…`, the
//!   listener captures it, and the exchange completes.
//! - **Manual paste** (`--manual`) — no local port is bound; the authorize URL
//!   is printed and the user opens it in any browser, then pastes the final
//!   redirected URL (containing `code` + `state`) back into the CLI. SSH /
//!   headless friendly.
//!
//! Tokens are persisted per-server under `~/.oneai/mcp_oauth/<server>.json` as
//! a single record carrying everything needed to refresh (`token_endpoint`,
//! `client_id`, optional `client_secret`, scopes) — so the transport-level
//! 401-retry in [`oneai_tool::McpConnection`] can refresh without consulting
//! the registry config. PKCE uses S256; `state` is a random hex CSRF token.
//!
//! Randomness comes from `uuid::Uuid::new_v4()` (already a workspace dep — no
//! new crypto RNG crate).

use std::path::PathBuf;

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use oneai_core::error::{OneAIError, Result};
use oneai_tool::McpOAuthTokenRefresher;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::form_urlencoded;

// ─── McpOAuthConfig ───────────────────────────────────────────────────────────

fn default_true() -> bool {
    true
}

/// Declarative, per-server OAuth 2.0 config. Lives under
/// `[servers.<name>]` in `~/.oneai/mcp_servers.toml` as an optional `oauth`
/// table. All fields have safe defaults so an entry can be as small as:
///
/// ```toml
/// [servers.my_remote]
/// transport = "streamable_http"
/// url = "https://api.example.com/mcp"
/// enabled = true
/// [servers.my_remote.oauth]
/// scopes = ["mcp:tools"]
/// ```
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct McpOAuthConfig {
    /// The OAuth-protected resource URL. Defaults to the transport `url` when
    /// omitted — i.e. the MCP server's own endpoint is the resource.
    #[serde(default)]
    pub resource_url: Option<String>,
    /// Requested scopes.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Static client id (skip dynamic registration). If absent and the auth
    /// server advertises a `registration_endpoint`, dynamic client
    /// registration is used (when `use_dynamic_registration` is true).
    #[serde(default)]
    pub client_id: Option<String>,
    /// Static client secret (confidential clients only). Leave empty for
    /// public PKCE flows.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Loopback redirect port. `None`/`0` = let the OS pick (loopback mode).
    /// Ignored in manual mode (a default port is used to keep `redirect_uri`
    /// stable for the paste flow).
    #[serde(default)]
    pub redirect_port: Option<u16>,
    /// Whether to attempt dynamic client registration when no static
    /// `client_id` is set and the server advertises a registration endpoint.
    #[serde(default = "default_true")]
    pub use_dynamic_registration: bool,
    /// Whether to send a PKCE `code_challenge` (S256). Almost always required
    /// for public clients; default true.
    #[serde(default = "default_true")]
    pub pkce: bool,
}

impl Default for McpOAuthConfig {
    fn default() -> Self {
        Self {
            resource_url: None,
            scopes: Vec::new(),
            client_id: None,
            client_secret: None,
            redirect_port: None,
            use_dynamic_registration: true,
            pkce: true,
        }
    }
}

/// Default loopback port used in manual mode (no binding — only a stable
/// `redirect_uri` for the paste flow).
const DEFAULT_MANUAL_PORT: u16 = 8273;

// ─── PKCE + state ─────────────────────────────────────────────────────────────

/// A PKCE verifier/challenge pair (S256).
#[derive(Debug, Clone)]
pub struct Pkce {
    pub code_verifier: String,
    pub code_challenge: String,
}

impl Pkce {
    /// Generate a fresh PKCE pair: 32 random bytes (two `Uuid::new_v4`s) →
    /// base64url-no-pad verifier; `code_challenge = base64url(sha256(verifier))`.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        bytes[..16].copy_from_slice(a.as_bytes());
        bytes[16..].copy_from_slice(b.as_bytes());
        let code_verifier = URL_SAFE_NO_PAD.encode(bytes);
        let mut hasher = Sha256::new();
        hasher.update(code_verifier.as_bytes());
        let digest = hasher.finalize();
        let code_challenge = URL_SAFE_NO_PAD.encode(digest);
        Self {
            code_verifier,
            code_challenge,
        }
    }
}

/// A random hex state token for CSRF protection (16 bytes).
pub fn generate_state() -> String {
    let a = uuid::Uuid::new_v4();
    let b = uuid::Uuid::new_v4();
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(a.as_bytes());
    bytes[16..].copy_from_slice(b.as_bytes());
    hex_encode(&bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

// ─── OAuth metadata ───────────────────────────────────────────────────────────

/// `/.well-known/oauth-protected-resource` response (RFC 9728).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct OAuthProtectedResource {
    #[serde(default)]
    pub resource: Option<String>,
    #[serde(default)]
    pub authorization_servers: Vec<String>,
}

/// `/.well-known/oauth-authorization-server` response (RFC 8414). Only the
/// fields the MCP flow uses are parsed; unknown fields are ignored.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct OAuthAuthorizationServer {
    #[serde(default)]
    pub issuer: Option<String>,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub registration_endpoint: Option<String>,
    #[serde(default)]
    pub revocation_endpoint: Option<String>,
    #[serde(default)]
    pub scopes_supported: Option<Vec<String>>,
    #[serde(default)]
    pub code_challenge_methods_supported: Option<Vec<String>>,
}

/// A registered OAuth client (static or dynamically registered).
#[derive(Debug, Clone)]
pub struct RegisteredClient {
    pub client_id: String,
    pub client_secret: Option<String>,
}

/// Dynamic client registration response (RFC 7591). Only the fields we use.
#[derive(Debug, Clone, serde::Deserialize)]
struct ClientRegistrationResponse {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
}

// ─── Stored tokens + token store ──────────────────────────────────────────────

/// Persisted token record. Carries the refresh context (token endpoint +
/// client credentials + scopes) so a 401-retry can refresh without the
/// originating `McpOAuthConfig`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OAuthStoredTokens {
    pub access_token: String,
    pub token_type: String,
    pub refresh_token: Option<String>,
    /// Wall-clock expiry. Refreshed proactively if within `EXPIRY_SKEW` of now.
    pub expires_at: Option<DateTime<Utc>>,
    pub scope: Option<String>,
    // ── refresh context ──
    pub token_endpoint: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub scopes: Vec<String>,
}

/// Clock skew tolerance for proactive refresh (seconds before expiry).
const EXPIRY_SKEW: i64 = 30;

impl OAuthStoredTokens {
    /// Whether the access token is already expired or about to expire.
    pub fn is_expired(&self) -> bool {
        match self.expires_at {
            Some(exp) => Utc::now() + Duration::seconds(EXPIRY_SKEW) >= exp,
            None => false, // no expiry known — treat as valid until a 401 says otherwise
        }
    }

    fn scope_string(&self) -> Option<String> {
        if self.scopes.is_empty() {
            self.scope.clone()
        } else {
            Some(self.scopes.join(" "))
        }
    }
}

/// File-backed per-server token store at `~/.oneai/mcp_oauth/<server>.json`.
#[derive(Debug, Clone)]
pub struct OAuthTokenStore {
    root: PathBuf,
}

impl OAuthTokenStore {
    /// Default store at `~/.oneai/mcp_oauth/`.
    pub fn default_store() -> Self {
        let root = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".oneai")
            .join("mcp_oauth");
        Self { root }
    }

    /// Store rooted at `root` (tests).
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn path_for(&self, server: &str) -> PathBuf {
        // Sanitize the server name into a filesystem-safe stem.
        let stem: String = server
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.root.join(format!("{}.json", stem))
    }

    /// Load a server's stored tokens, if any.
    pub fn load(&self, server: &str) -> Option<OAuthStoredTokens> {
        let path = self.path_for(server);
        let content = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str::<OAuthStoredTokens>(&content).ok()
    }

    /// Persist a token record.
    pub fn save(&self, server: &str, tokens: &OAuthStoredTokens) -> Result<()> {
        if !self.root.exists() {
            std::fs::create_dir_all(&self.root).map_err(|e| {
                OneAIError::Persistence(format!("Failed to create OAuth token dir: {}", e))
            })?;
        }
        let path = self.path_for(server);
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_string_pretty(tokens).map_err(|e| {
            OneAIError::Serialization(format!("Failed to serialize OAuth tokens: {}", e))
        })?;
        std::fs::write(&tmp, body)
            .map_err(|e| OneAIError::Persistence(format!("Failed to write OAuth tokens: {}", e)))?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            OneAIError::Persistence(format!("Failed to finalize OAuth tokens: {}", e))
        })?;
        Ok(())
    }

    /// Delete a server's token record (logout).
    pub fn delete(&self, server: &str) -> Result<()> {
        let path = self.path_for(server);
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| {
                OneAIError::Persistence(format!("Failed to delete OAuth tokens: {}", e))
            })?;
        }
        Ok(())
    }
}

// ─── OAuth flow ───────────────────────────────────────────────────────────────

/// Drives the MCP OAuth 2.0 authorization-code-with-PKCE flow for one server.
pub struct McpOAuthFlow {
    server_name: String,
    config: McpOAuthConfig,
    resource_url: String,
    http: reqwest::Client,
    store: OAuthTokenStore,
}

impl McpOAuthFlow {
    /// Build a flow for `server_name`. `resource_url` is the OAuth-protected
    /// resource (normally the MCP server's own HTTP/SSE url); it overrides
    /// `config.resource_url` when the latter is `None`. The store is where
    /// the resulting tokens are persisted.
    pub fn new(
        server_name: String,
        mut config: McpOAuthConfig,
        resource_url: String,
        store: OAuthTokenStore,
    ) -> Self {
        if config.resource_url.is_none() {
            config.resource_url = Some(resource_url.clone());
        }
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            server_name,
            config,
            resource_url,
            http,
            store,
        }
    }

    /// Discover the authorization-server metadata for this resource.
    ///
    /// GET `<resource>/.well-known/oauth-protected-resource` → list of
    /// authorization servers; then GET `<auth_server>/.well-known/
    /// oauth-authorization-server` → endpoints. If the protected-resource
    /// doc is missing, the resource URL itself is treated as the issuer
    /// (some MCP servers collapse the two).
    pub async fn discover(&self) -> Result<OAuthAuthorizationServer> {
        let protected = format!(
            "{}/.well-known/oauth-protected-resource",
            self.resource_url.trim_end_matches('/')
        );
        let auth_servers: Vec<String> = match self.http.get(&protected).send().await {
            Ok(resp) if resp.status().is_success() => {
                let v: serde_json::Value = resp.json().await.map_err(|e| {
                    OneAIError::Provider(format!("OAuth metadata parse error: {}", e))
                })?;
                v.get("authorization_servers")
                    .and_then(|s| s.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|u| u.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default()
            }
            _ => {
                // Fallback: the resource URL is itself the authorization server.
                vec![self.resource_url.clone()]
            }
        };

        let issuer = auth_servers
            .first()
            .cloned()
            .unwrap_or_else(|| self.resource_url.clone());

        let metadata_url = format!(
            "{}/.well-known/oauth-authorization-server",
            issuer.trim_end_matches('/')
        );
        let resp =
            self.http.get(&metadata_url).send().await.map_err(|e| {
                OneAIError::Provider(format!("OAuth discovery request failed: {}", e))
            })?;
        if !resp.status().is_success() {
            return Err(OneAIError::Provider(format!(
                "OAuth discovery at {} returned status {}",
                metadata_url,
                resp.status().as_u16()
            )));
        }
        let metadata: OAuthAuthorizationServer = resp.json().await.map_err(|e| {
            OneAIError::Provider(format!("OAuth authorization-server metadata parse: {}", e))
        })?;
        Ok(metadata)
    }

    /// Resolve a client: static `client_id` if configured, else dynamic
    /// registration (when the server advertises a registration endpoint and
    /// `use_dynamic_registration` is true). Errors if no client can be
    /// obtained.
    pub async fn ensure_client(
        &self,
        metadata: &OAuthAuthorizationServer,
        redirect_uri: &str,
    ) -> Result<RegisteredClient> {
        if let Some(id) = self.config.client_id.clone() {
            return Ok(RegisteredClient {
                client_id: id,
                client_secret: self.config.client_secret.clone(),
            });
        }
        if !self.config.use_dynamic_registration {
            return Err(OneAIError::Config(
                "No client_id configured and dynamic registration disabled".to_string(),
            ));
        }
        let endpoint = metadata.registration_endpoint.as_deref().ok_or_else(|| {
            OneAIError::Config(
                "No client_id configured and server advertises no registration_endpoint"
                    .to_string(),
            )
        })?;
        self.dynamic_register(endpoint, redirect_uri).await
    }

    async fn dynamic_register(
        &self,
        endpoint: &str,
        redirect_uri: &str,
    ) -> Result<RegisteredClient> {
        let body = serde_json::json!({
            "client_name": format!("oneai-mcp-{}", self.server_name),
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "token_endpoint_auth_method": "none",
            "scope": self.config.scopes.join(" "),
        });
        let resp = self
            .http
            .post(endpoint)
            .json(&body)
            .send()
            .await
            .map_err(|e| OneAIError::Provider(format!("Dynamic registration failed: {}", e)))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(OneAIError::Provider(format!(
                "Dynamic registration returned status {}: {}",
                status, body
            )));
        }
        let reg: ClientRegistrationResponse = resp.json().await.map_err(|e| {
            OneAIError::Provider(format!("Dynamic registration parse error: {}", e))
        })?;
        Ok(RegisteredClient {
            client_id: reg.client_id,
            client_secret: reg.client_secret,
        })
    }

    /// Build the authorization URL with PKCE + state. Returns the URL string.
    pub fn build_authorize_url(
        &self,
        metadata: &OAuthAuthorizationServer,
        pkce: &Pkce,
        state: &str,
        redirect_uri: &str,
        client_id: &str,
    ) -> String {
        let mut params = form_urlencoded::Serializer::new(String::new());
        params.append_pair("response_type", "code");
        params.append_pair("client_id", client_id);
        params.append_pair("redirect_uri", redirect_uri);
        params.append_pair("state", state);
        if self.config.pkce {
            params.append_pair("code_challenge", &pkce.code_challenge);
            params.append_pair("code_challenge_method", "S256");
        }
        if !self.config.scopes.is_empty() {
            params.append_pair("scope", &self.config.scopes.join(" "));
        }
        format!(
            "{}?{}",
            metadata.authorization_endpoint.trim_end_matches('?'),
            params.finish()
        )
    }

    /// Exchange an authorization code for tokens, persisting the result.
    pub async fn exchange_code(
        &self,
        metadata: &OAuthAuthorizationServer,
        code: &str,
        pkce: &Pkce,
        redirect_uri: &str,
        client: &RegisteredClient,
    ) -> Result<OAuthStoredTokens> {
        let body = {
            let mut form = form_urlencoded::Serializer::new(String::new());
            form.append_pair("grant_type", "authorization_code");
            form.append_pair("code", code);
            form.append_pair("redirect_uri", redirect_uri);
            form.append_pair("client_id", &client.client_id);
            if self.config.pkce {
                form.append_pair("code_verifier", &pkce.code_verifier);
            }
            if let Some(secret) = client.client_secret.as_deref() {
                form.append_pair("client_secret", secret);
            }
            form.finish()
        };

        let resp = self
            .http
            .post(&metadata.token_endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| OneAIError::Provider(format!("Token exchange failed: {}", e)))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            return Err(OneAIError::Provider(format!(
                "Token exchange returned status {}: {}",
                status, text
            )));
        }
        let tokens = parse_token_response(
            resp.json()
                .await
                .map_err(|e| OneAIError::Provider(format!("Token exchange parse error: {}", e)))?,
            &metadata.token_endpoint,
            &client.client_id,
            client.client_secret.as_deref(),
            &self.config.scopes,
        );
        self.store.save(&self.server_name, &tokens)?;
        Ok(tokens)
    }

    /// Run the loopback-redirect login flow: bind a local port, build the
    /// authorize URL, print + open the browser, capture the redirect, exchange
    /// the code. Returns the persisted tokens.
    pub async fn login_loopback(&self) -> Result<OAuthStoredTokens> {
        let port = self.config.redirect_port.unwrap_or(0);
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|e| OneAIError::Network(format!("Bind loopback OAuth listener: {}", e)))?;
        let actual_port = listener
            .local_addr()
            .map_err(|e| OneAIError::Network(format!("Loopback addr: {}", e)))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{}/callback", actual_port);

        self.run_login(redirect_uri, LoginMode::Loopback(listener))
            .await
    }

    /// Run the manual paste login flow: print the authorize URL, read the
    /// redirected URL from stdin, parse code + state, exchange.
    pub async fn login_manual(&self) -> Result<OAuthStoredTokens> {
        let port = self.config.redirect_port.unwrap_or(DEFAULT_MANUAL_PORT);
        let redirect_uri = format!("http://127.0.0.1:{}/callback", port);
        self.run_login(redirect_uri, LoginMode::Manual).await
    }

    async fn run_login(&self, redirect_uri: String, mode: LoginMode) -> Result<OAuthStoredTokens> {
        let metadata = self.discover().await?;
        let client = self.ensure_client(&metadata, &redirect_uri).await?;
        let pkce = if self.config.pkce {
            Some(Pkce::generate())
        } else {
            None
        };
        let state = generate_state();
        let pkce_for_url = pkce.as_ref().cloned().unwrap_or_else(|| Pkce {
            code_verifier: String::new(),
            code_challenge: String::new(),
        });
        let auth_url = self.build_authorize_url(
            &metadata,
            &pkce_for_url,
            &state,
            &redirect_uri,
            &client.client_id,
        );

        println!(
            "\n🔐 Open this URL in a browser to authorize:\n{}\n",
            auth_url
        );
        try_open_browser(&auth_url);

        let (code, _returned_state) = match mode {
            LoginMode::Loopback(listener) => capture_loopback_redirect(listener, &state).await?,
            LoginMode::Manual => {
                println!("After authorizing, paste the full redirected URL here:");
                let line = read_line().await?;
                parse_redirect_url(&line, &state)?
            }
        };

        let pkce_for_exchange = pkce.unwrap_or_else(|| Pkce {
            code_verifier: String::new(),
            code_challenge: String::new(),
        });
        let tokens = self
            .exchange_code(&metadata, &code, &pkce_for_exchange, &redirect_uri, &client)
            .await?;
        println!("✅ OAuth tokens stored for '{}'.", self.server_name);
        Ok(tokens)
    }

    /// Load stored tokens; if absent or refreshable, return the current
    /// access token. `None` = no stored tokens (caller should run `login`).
    pub fn stored_token(&self) -> Option<OAuthStoredTokens> {
        self.store.load(&self.server_name)
    }

    /// Refresh the stored tokens for this server using the stored refresh
    /// token, persisting the new record. Errors propagate.
    pub async fn refresh_stored(&self) -> Result<OAuthStoredTokens> {
        let tokens = self.store.load(&self.server_name).ok_or_else(|| {
            OneAIError::Config(format!("No stored OAuth tokens for '{}'", self.server_name))
        })?;
        let refreshed = refresh_token(&self.http, tokens).await?;
        self.store.save(&self.server_name, &refreshed)?;
        Ok(refreshed)
    }
}

// ─── Login helpers ────────────────────────────────────────────────────────────

enum LoginMode {
    Loopback(TcpListener),
    Manual,
}

/// Capture the authorization-code redirect on a bound loopback listener.
/// Accepts one HTTP request, validates `state`, returns `(code, state)`.
async fn capture_loopback_redirect(
    listener: TcpListener,
    expected_state: &str,
) -> Result<(String, String)> {
    let (mut stream, _) = listener
        .accept()
        .await
        .map_err(|e| OneAIError::Network(format!("OAuth redirect accept: {}", e)))?;
    let mut buf = [0u8; 4096];
    let n = stream
        .read(&mut buf)
        .await
        .map_err(|e| OneAIError::Network(format!("OAuth redirect read: {}", e)))?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let first_line = request.lines().next().unwrap_or("");
    // Respond (success page) regardless of parse outcome so the browser shows
    // a clean page rather than a connection reset.
    let body = "<html><body>OneAI MCP OAuth — you can close this window.</body></html>";
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.flush().await;

    // `first_line` looks like: `GET /callback?code=...&state=... HTTP/1.1`
    let path = first_line.split_whitespace().nth(1).unwrap_or("");
    parse_redirect_query(path, expected_state)
}

/// Parse `code` + `state` out of a query string (`/callback?code=..&state=..`).
fn parse_redirect_query(path_with_query: &str, expected_state: &str) -> Result<(String, String)> {
    let query = path_with_query
        .split_once('?')
        .map(|(_, q)| q)
        .unwrap_or("");
    let mut code = None;
    let mut state = None;
    for (k, v) in form_urlencoded::parse(query.as_bytes()) {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            _ => {}
        }
    }
    let code =
        code.ok_or_else(|| OneAIError::Provider("OAuth redirect missing 'code'".to_string()))?;
    let state =
        state.ok_or_else(|| OneAIError::Provider("OAuth redirect missing 'state'".to_string()))?;
    if state != expected_state {
        return Err(OneAIError::Provider(
            "OAuth redirect 'state' mismatch — possible CSRF".to_string(),
        ));
    }
    Ok((code, state))
}

/// Parse a full redirected URL (manual paste) into `(code, state)`.
fn parse_redirect_url(url: &str, expected_state: &str) -> Result<(String, String)> {
    // Find the query portion (after `?`), stripping any fragment.
    let after_question = url.split_once('?').map(|(_, q)| q).unwrap_or("");
    let query = after_question
        .split_once('#')
        .map(|(q, _)| q)
        .unwrap_or(after_question);
    parse_redirect_query(&format!("/x?{}", query), expected_state)
}

/// Best-effort, platform-specific browser open. Failure is silent — the URL
/// is always printed alongside.
fn try_open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let cmd = ("open", Vec::from([url.to_string()]));
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = ("xdg-open", Vec::from([url.to_string()]));
    #[cfg(target_os = "windows")]
    let cmd = (
        "cmd",
        Vec::from(["/C".to_string(), "start".to_string(), url.to_string()]),
    );
    #[cfg(not(any(target_os = "macos", target_os = "windows", unix)))]
    let cmd: (&str, Vec<String>) = ("", Vec::new());

    if cmd.0.is_empty() {
        return;
    }
    let _ = std::process::Command::new(cmd.0).args(&cmd.1).spawn();
}

/// Read one line from stdin without blocking the async runtime.
async fn read_line() -> Result<String> {
    tokio::task::spawn_blocking(|| {
        use std::io::Read;
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .map_err(|e| OneAIError::Provider(format!("stdin read failed: {}", e)))?;
        Ok(input.trim().to_string())
    })
    .await
    .map_err(|e| OneAIError::Provider(format!("stdin spawn failed: {}", e)))?
}

// ─── Token response parsing + refresh ─────────────────────────────────────────

/// A standard OAuth 2.0 token endpoint response.
#[derive(Debug, Clone, serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    token_type: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    error: Option<String>,
}

fn parse_token_response(
    resp: TokenResponse,
    token_endpoint: &str,
    client_id: &str,
    client_secret: Option<&str>,
    scopes: &[String],
) -> OAuthStoredTokens {
    let expires_at = resp
        .expires_in
        .map(|secs| Utc::now() + Duration::seconds(secs as i64));
    OAuthStoredTokens {
        access_token: resp.access_token,
        token_type: if resp.token_type.is_empty() {
            "Bearer".to_string()
        } else {
            resp.token_type
        },
        refresh_token: resp.refresh_token,
        expires_at,
        scope: resp.scope,
        token_endpoint: token_endpoint.to_string(),
        client_id: client_id.to_string(),
        client_secret: client_secret.map(str::to_string),
        scopes: scopes.to_vec(),
    }
}

/// Refresh a token record using its stored refresh token + endpoint. The
/// returned record carries forward the refresh context. Errors if no
/// refresh token is available or the server rejects the refresh.
pub async fn refresh_token(
    http: &reqwest::Client,
    tokens: OAuthStoredTokens,
) -> Result<OAuthStoredTokens> {
    let refresh_token = tokens
        .refresh_token
        .clone()
        .ok_or_else(|| OneAIError::Provider("No refresh_token available to refresh".to_string()))?;
    let body = {
        let mut form = form_urlencoded::Serializer::new(String::new());
        form.append_pair("grant_type", "refresh_token");
        form.append_pair("refresh_token", &refresh_token);
        form.append_pair("client_id", &tokens.client_id);
        if let Some(secret) = tokens.client_secret.as_deref() {
            form.append_pair("client_secret", secret);
        }
        if let Some(scope) = tokens.scope_string() {
            form.append_pair("scope", &scope);
        }
        form.finish()
    };

    let resp = http
        .post(&tokens.token_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| OneAIError::Provider(format!("Token refresh failed: {}", e)))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        return Err(OneAIError::Provider(format!(
            "Token refresh returned status {}: {}",
            status, text
        )));
    }
    let tr: TokenResponse = resp
        .json()
        .await
        .map_err(|e| OneAIError::Provider(format!("Token refresh parse error: {}", e)))?;
    // RFC 6749 §5.1: refreshes MAY omit fields the server doesn't rotate —
    // carry forward the prior refresh token + scope when absent.
    let merged_refresh = tr.refresh_token.or(Some(refresh_token));
    let merged_scope = tr.scope.or(tokens.scope.clone());
    let expires_at = tr
        .expires_in
        .map(|secs| Utc::now() + Duration::seconds(secs as i64));
    Ok(OAuthStoredTokens {
        access_token: tr.access_token,
        token_type: if tr.token_type.is_empty() {
            "Bearer".to_string()
        } else {
            tr.token_type
        },
        refresh_token: merged_refresh,
        expires_at: expires_at.or(tokens.expires_at),
        scope: merged_scope,
        token_endpoint: tokens.token_endpoint,
        client_id: tokens.client_id,
        client_secret: tokens.client_secret,
        scopes: tokens.scopes,
    })
}

// ─── 401-refresher (transport-layer hook) ─────────────────────────────────────

/// Transport-layer OAuth token refresher — implements
/// [`oneai_tool::McpOAuthTokenRefresher`] so `McpConnection` can recover from
/// a 401 by refreshing + retrying once. Holds only the token store; the
/// stored record carries the refresh endpoint + client credentials, so no
/// per-server config is needed at call time.
#[derive(Debug, Clone)]
pub struct McpOAuthTokenRefresherImpl {
    store: OAuthTokenStore,
    http: reqwest::Client,
}

impl McpOAuthTokenRefresherImpl {
    pub fn new(store: OAuthTokenStore) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { store, http }
    }
}

#[async_trait]
impl McpOAuthTokenRefresher for McpOAuthTokenRefresherImpl {
    async fn refresh_token(&self, server_name: &str) -> Result<Option<String>> {
        let tokens = match self.store.load(server_name) {
            Some(t) => t,
            None => return Ok(None), // no OAuth for this server — surface the 401
        };
        // Only attempt a refresh if we actually have a refresh token.
        if tokens.refresh_token.is_none() {
            return Ok(None);
        }
        let refreshed = refresh_token(&self.http, tokens).await?;
        self.store.save(server_name, &refreshed)?;
        Ok(Some(refreshed.access_token))
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[test]
    fn test_pkce_verifier_is_base64url_no_pad() {
        let pkce = Pkce::generate();
        assert!(!pkce.code_verifier.is_empty());
        // base64url-no-pad contains no '=' padding.
        assert!(!pkce.code_verifier.contains('='));
        assert!(!pkce.code_verifier.contains('+'));
        assert!(!pkce.code_verifier.contains('/'));
        // Challenge = base64url(sha256(verifier)) — recompute to verify.
        let mut hasher = Sha256::new();
        hasher.update(pkce.code_verifier.as_bytes());
        let expected = URL_SAFE_NO_PAD.encode(hasher.finalize());
        assert_eq!(pkce.code_challenge, expected);
    }

    #[test]
    fn test_pkce_is_unique() {
        let a = Pkce::generate();
        let b = Pkce::generate();
        assert_ne!(a.code_verifier, b.code_verifier);
    }

    #[test]
    fn test_state_is_hex_and_unique() {
        let a = generate_state();
        let b = generate_state();
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(a.len(), 64); // 32 bytes → 64 hex chars
    }

    #[test]
    fn test_build_authorize_url_has_required_params() {
        let cfg = McpOAuthConfig {
            scopes: vec!["mcp:tools".to_string()],
            ..Default::default()
        };
        let flow = McpOAuthFlow::new(
            "srv".to_string(),
            cfg,
            "https://res.example.com/mcp".to_string(),
            OAuthTokenStore::new(PathBuf::from("/tmp/nonexistent_oauth_store")),
        );
        let metadata = OAuthAuthorizationServer {
            issuer: Some("https://auth.example.com".to_string()),
            authorization_endpoint: "https://auth.example.com/authorize".to_string(),
            token_endpoint: "https://auth.example.com/token".to_string(),
            registration_endpoint: None,
            revocation_endpoint: None,
            scopes_supported: None,
            code_challenge_methods_supported: None,
        };
        let pkce = Pkce::generate();
        let state = generate_state();
        let url = flow.build_authorize_url(
            &metadata,
            &pkce,
            &state,
            "http://127.0.0.1:5/callback",
            "client-123",
        );
        assert!(url.starts_with("https://auth.example.com/authorize?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=client-123"));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state="));
        assert!(url.contains("scope=mcp%3Atools"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A5%2Fcallback"));
    }

    #[test]
    fn test_parse_redirect_query_extracts_code_and_state() {
        let state = generate_state();
        let path = format!("/callback?code=abcd1234&state={}", state);
        let (code, returned_state) = parse_redirect_query(&path, &state).unwrap();
        assert_eq!(code, "abcd1234");
        assert_eq!(returned_state, state);
    }

    #[test]
    fn test_parse_redirect_query_state_mismatch_errors() {
        let path = "/callback?code=abcd&state=wrong";
        let result = parse_redirect_query(path, "expected");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_redirect_url_handles_fragment() {
        let state = generate_state();
        let url = format!(
            "http://127.0.0.1:5/callback?code=xyz&state={}#fragment",
            state
        );
        let (code, returned_state) = parse_redirect_url(&url, &state).unwrap();
        assert_eq!(code, "xyz");
        assert_eq!(returned_state, state);
    }

    #[test]
    fn test_token_store_roundtrip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = OAuthTokenStore::new(tmp.path().to_path_buf());
        let tokens = OAuthStoredTokens {
            access_token: "access-123".to_string(),
            token_type: "Bearer".to_string(),
            refresh_token: Some("refresh-456".to_string()),
            expires_at: Some(Utc::now() + Duration::seconds(3600)),
            scope: Some("mcp:tools".to_string()),
            token_endpoint: "https://auth.example.com/token".to_string(),
            client_id: "cid".to_string(),
            client_secret: None,
            scopes: vec!["mcp:tools".to_string()],
        };
        store.save("my_server", &tokens).unwrap();
        let loaded = store.load("my_server").unwrap();
        assert_eq!(loaded.access_token, "access-123");
        assert_eq!(loaded.refresh_token.as_deref(), Some("refresh-456"));
        assert_eq!(loaded.client_id, "cid");
        // Server name is sanitized for the filename.
        assert!(!loaded.token_endpoint.is_empty());

        store.delete("my_server").unwrap();
        assert!(store.load("my_server").is_none());
    }

    #[test]
    fn test_token_store_sanitizes_server_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = OAuthTokenStore::new(tmp.path().to_path_buf());
        let tokens = OAuthStoredTokens {
            access_token: "a".to_string(),
            token_type: "Bearer".to_string(),
            refresh_token: None,
            expires_at: None,
            scope: None,
            token_endpoint: "https://x/t".to_string(),
            client_id: "c".to_string(),
            client_secret: None,
            scopes: vec![],
        };
        store.save("server/../weird name", &tokens).unwrap();
        // The on-disk filename must stay inside the store root (no `..` escape).
        let entries: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().collect();
        assert_eq!(entries.len(), 1);
        // And it round-trips.
        let loaded = store.load("server/../weird name").unwrap();
        assert_eq!(loaded.access_token, "a");
    }

    #[test]
    fn test_stored_tokens_is_expired() {
        let past = Utc::now() - Duration::seconds(60);
        let future = Utc::now() + Duration::seconds(3600);
        let expired = OAuthStoredTokens {
            access_token: "a".to_string(),
            token_type: "Bearer".to_string(),
            refresh_token: None,
            expires_at: Some(past),
            scope: None,
            token_endpoint: "https://x/t".to_string(),
            client_id: "c".to_string(),
            client_secret: None,
            scopes: vec![],
        };
        let fresh = OAuthStoredTokens {
            expires_at: Some(future),
            ..expired.clone()
        };
        assert!(expired.is_expired());
        assert!(!fresh.is_expired());
    }

    #[test]
    fn test_config_defaults() {
        let cfg = McpOAuthConfig::default();
        assert!(cfg.use_dynamic_registration);
        assert!(cfg.pkce);
        assert!(cfg.scopes.is_empty());
        assert!(cfg.client_id.is_none());
    }

    #[test]
    fn test_config_serde_roundtrip() {
        let toml_str = r#"
resource_url = "https://res.example.com/mcp"
scopes = ["mcp:tools", "read"]
client_id = "abc"
client_secret = "sec"
redirect_port = 8273
use_dynamic_registration = false
pkce = true
"#;
        let cfg: McpOAuthConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            cfg.resource_url.as_deref(),
            Some("https://res.example.com/mcp")
        );
        assert_eq!(cfg.scopes, vec!["mcp:tools", "read"]);
        assert!(!cfg.use_dynamic_registration);
        assert!(cfg.pkce);
        let again = toml::to_string(&cfg).unwrap();
        let _re: McpOAuthConfig = toml::from_str(&again).unwrap();
    }

    // ── Mock OAuth server (raw TcpListener) ─────────────────────────────────

    /// Spawn a tiny HTTP server that replies to the OAuth endpoints used in
    /// the exchange/refresh tests. Returns its base URL + the server handle.
    async fn spawn_mock_oauth_server() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{}", addr);
        let handle = tokio::spawn(async move {
            // Serve up to a few requests; the tests below exercise the exact
            // endpoint they need and then drop the handle.
            for _ in 0..8 {
                let (mut sock, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let req = read_http_request(&mut sock).await;
                let first = req.lines().next().unwrap_or("");
                let body = if first.contains("/token") {
                    // Extract grant_type to distinguish exchange from refresh.
                    if req.contains("grant_type=refresh_token") {
                        r#"{"access_token":"rotated","token_type":"Bearer","refresh_token":"rt2","expires_in":3600,"scope":"mcp:tools"}"#
                    } else {
                        r#"{"access_token":"access-1","token_type":"Bearer","refresh_token":"rt1","expires_in":3600,"scope":"mcp:tools"}"#
                    }
                } else {
                    "{}"
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (base, handle)
    }

    /// Read a full HTTP request (headers + Content-Length body) from a stream,
    /// looping until the whole request is buffered. The form bodies in these
    /// tests are small but TCP may split them across packets.
    async fn read_http_request<S: AsyncReadExt + Unpin>(stream: &mut S) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            // Once we have headers, parse Content-Length and stop once we've
            // buffered the full body.
            if let Some(header_end) = find_subslice(&buf, b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
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
                let body_start = header_end + 4;
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

    #[tokio::test]
    async fn test_refresh_token_against_mock_server() {
        let (base, _h) = spawn_mock_oauth_server().await;
        let http = reqwest::Client::new();
        let tokens = OAuthStoredTokens {
            access_token: "stale".to_string(),
            token_type: "Bearer".to_string(),
            refresh_token: Some("rt1".to_string()),
            expires_at: Some(Utc::now() - Duration::seconds(60)),
            scope: Some("mcp:tools".to_string()),
            token_endpoint: format!("{}/token", base),
            client_id: "cid".to_string(),
            client_secret: None,
            scopes: vec!["mcp:tools".to_string()],
        };
        let refreshed = refresh_token(&http, tokens).await.unwrap();
        assert_eq!(refreshed.access_token, "rotated");
        assert_eq!(refreshed.refresh_token.as_deref(), Some("rt2"));
        assert!(refreshed.expires_at.is_some());
        // Refresh context is carried forward.
        assert_eq!(refreshed.client_id, "cid");
        assert!(refreshed.token_endpoint.contains("/token"));
    }

    #[tokio::test]
    async fn test_refresher_impl_returns_none_when_no_store() {
        let store = OAuthTokenStore::new(PathBuf::from("/tmp/nonexistent_oauth_store_xx"));
        let refresher = McpOAuthTokenRefresherImpl::new(store);
        let out = refresher.refresh_token("no_such_server").await.unwrap();
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn test_refresher_impl_refreshes_stored_tokens() {
        let (base, _h) = spawn_mock_oauth_server().await;
        let tmp = tempfile::TempDir::new().unwrap();
        let store = OAuthTokenStore::new(tmp.path().to_path_buf());
        let tokens = OAuthStoredTokens {
            access_token: "stale".to_string(),
            token_type: "Bearer".to_string(),
            refresh_token: Some("rt1".to_string()),
            expires_at: Some(Utc::now() - Duration::seconds(60)),
            scope: Some("mcp:tools".to_string()),
            token_endpoint: format!("{}/token", base),
            client_id: "cid".to_string(),
            client_secret: None,
            scopes: vec!["mcp:tools".to_string()],
        };
        store.save("srv", &tokens).unwrap();
        let refresher = McpOAuthTokenRefresherImpl::new(store.clone());
        let out = refresher.refresh_token("srv").await.unwrap();
        assert_eq!(out.as_deref(), Some("rotated"));
        // Persisted.
        let reloaded = store.load("srv").unwrap();
        assert_eq!(reloaded.access_token, "rotated");
    }

    #[tokio::test]
    async fn test_refresher_impl_returns_none_without_refresh_token() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = OAuthTokenStore::new(tmp.path().to_path_buf());
        let tokens = OAuthStoredTokens {
            access_token: "no_refresh".to_string(),
            token_type: "Bearer".to_string(),
            refresh_token: None,
            expires_at: None,
            scope: None,
            token_endpoint: "https://x/t".to_string(),
            client_id: "c".to_string(),
            client_secret: None,
            scopes: vec![],
        };
        store.save("srv", &tokens).unwrap();
        let refresher = McpOAuthTokenRefresherImpl::new(store);
        let out = refresher.refresh_token("srv").await.unwrap();
        assert!(out.is_none()); // surface the 401
    }

    // Silence unused warnings for platform-specific helpers in tests.
    #[test]
    fn _cover_browser_open() {
        // `try_open_browser` is best-effort and platform-gated; exercise the
        // path so it is not flagged as dead code on platforms without a
        // browser launcher.
        try_open_browser("http://example.com");
    }
}
