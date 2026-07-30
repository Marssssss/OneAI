//! Feishu (Lark) bot adapter — webhook inbound + REST send outbound.
//!
//! ## Inbound (event subscription callback, `POST /gateway/feishu`)
//! - **URL verification challenge**: Feishu first sends
//!   `{"challenge":"..","token":"..","type":"url_verification"}`; echo back
//!   `{"challenge":".."}` to complete URL registration.
//! - **Signature**: headers `X-Lark-Request-Timestamp` / `X-Lark-Request-Nonce`
//!   / `X-Lark-Signature`. The signature is plain `SHA-256(timestamp + nonce +
//!   body + verification_token)`, hex-encoded. (Feishu's documented formula uses
//!   the `encrypt` field; in plaintext mode — no `encrypt_key` — the body string
//!   stands in for `encrypt`.)
//! - **Encrypted events** (when `encrypt_key` is configured) arrive as
//!   `{"encrypt":"<base64>"}`. [`decrypt_event`] does AES-256-CBC
//!   (key = SHA256(encrypt_key), IV = first 16 bytes, PKCS7) and the adapter
//!   parses the decrypted envelope.
//! - **im.message.receive_v1**: the text-message event. Parsed to a
//!   [`MessageEvent`] whose `channel.raw` is the `chat_id` and `sender.id` is
//!   the sender's `open_id`.
//!
//! ## Outbound
//! `tenant_access_token` (cached with expiry) → `POST
//! open.feishu.cn/open-apis/im/v1/messages?receive_id_type=chat_id`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use crate::error::{GatewayError, Result};
use crate::event::{ChannelId, MessageEvent, Sender};
use crate::platform::MessagePlatform;
use crate::web::{WebhookAck, WebhookHandler};

/// Feishu app credentials + verification token.
#[derive(Clone)]
pub struct FeishuConfig {
    pub app_id: String,
    pub app_secret: String,
    /// The app's "Verification Token" (event subscription page).
    pub verification_token: String,
    /// The app's "Encrypt Key" (event subscription page). When set, Feishu
    /// delivers events as `{"encrypt":"<base64 AES-256-CBC>"}` and the adapter
    /// decrypts with [`decrypt_event`]. `None` = plaintext mode.
    pub encrypt_key: Option<String>,
    /// Base URL — `https://open.feishu.cn` (Lark: `https://open.larksuite.com`).
    pub base_url: String,
}

impl FeishuConfig {
    pub fn from_env() -> Option<Self> {
        Some(Self {
            app_id: std::env::var("FEISHU_APP_ID").ok()?,
            app_secret: std::env::var("FEISHU_APP_SECRET").ok()?,
            verification_token: std::env::var("FEISHU_VERIFY_TOKEN")
                .ok()
                .unwrap_or_default(),
            encrypt_key: std::env::var("FEISHU_ENCRYPT_KEY")
                .ok()
                .filter(|s| !s.is_empty()),
            base_url: std::env::var("FEISHU_BASE_URL")
                .unwrap_or_else(|_| "https://open.feishu.cn".to_string()),
        })
    }
}

/// Verify the Feishu `X-Lark-Signature` over a plaintext-mode body.
///
/// `signature_header` is the value of `X-Lark-Signature`. Returns `true` if it
/// matches `sha256(timestamp + nonce + body + verification_token)`. When
/// `verification_token` is empty, verification is skipped (returns `true`) —
/// for local dev only; production should always set the token.
pub fn verify_feishu_signature(
    timestamp: &str,
    nonce: &str,
    body: &str,
    verification_token: &str,
    signature_header: &str,
) -> bool {
    if verification_token.is_empty() {
        return true;
    }
    let mut hasher = Sha256::new();
    hasher.update(timestamp.as_bytes());
    hasher.update(nonce.as_bytes());
    hasher.update(body.as_bytes());
    hasher.update(verification_token.as_bytes());
    let digest = hasher.finalize();
    let computed = hex::encode(digest);
    // constant-time-ish compare via simple eq (signature is hex; not secret-ish here)
    computed == signature_header
}

/// Decrypt a Feishu `encrypt` payload (encrypt_key mode).
///
/// Feishu's formula: `encrypt = base64( AES-256-CBC( plaintext, key =
/// SHA256(encrypt_key), iv = first 16 bytes ) )` with PKCS7 padding. Returns
/// the plaintext JSON event envelope.
pub fn decrypt_event(encrypt_b64: &str, encrypt_key: &str) -> Result<String> {
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::{BlockDecrypt, KeyInit};
    use aes::Aes256;
    use base64::Engine;
    use sha2::{Digest, Sha256};

    // key = SHA256(encrypt_key) → 32 bytes (AES-256).
    let mut hasher = Sha256::new();
    hasher.update(encrypt_key.as_bytes());
    let key = hasher.finalize();

    let ciphertext = base64::engine::general_purpose::STANDARD
        .decode(encrypt_b64.as_bytes())
        .map_err(|e| GatewayError::Parse(format!("encrypt base64 decode: {e}")))?;
    // IV (16) + at least one block; total length must be a multiple of 16.
    if ciphertext.len() < 32 || ciphertext.len() % 16 != 0 {
        return Err(GatewayError::Parse(format!(
            "bad encrypted payload length: {} bytes",
            ciphertext.len()
        )));
    }
    let (iv, body) = ciphertext.split_at(16);

    let cipher = Aes256::new(GenericArray::from_slice(&key));
    let mut plain = Vec::with_capacity(body.len());
    let mut prev: &[u8] = iv;
    for block in body.chunks(16) {
        let mut buf = GenericArray::clone_from_slice(block);
        cipher.decrypt_block(&mut buf);
        // CBC: XOR the decrypted block with the previous ciphertext block
        // (or the IV for the first block).
        for (b, p) in buf.iter_mut().zip(prev.iter()) {
            *b ^= p;
        }
        plain.extend_from_slice(&buf);
        prev = block;
    }

    // PKCS7 unpad.
    let pad = *plain.last().unwrap_or(&0) as usize;
    if pad == 0 || pad > 16 || plain.len() < pad {
        return Err(GatewayError::Parse(format!("bad PKCS7 padding: {pad}")));
    }
    if !plain[plain.len() - pad..]
        .iter()
        .all(|&b| b as usize == pad)
    {
        return Err(GatewayError::Parse("inconsistent PKCS7 padding".into()));
    }
    plain.truncate(plain.len() - pad);
    String::from_utf8(plain)
        .map_err(|e| GatewayError::Parse(format!("decrypt plaintext not utf8: {e}")))
}

/// Handle a (possibly-decrypted) event envelope: echo `url_verification`
/// challenges, else parse an `im.message.receive_v1` event.
fn ack_envelope(value: &serde_json::Value) -> Result<WebhookAck> {
    if value.get("type").and_then(|v| v.as_str()) == Some("url_verification") {
        let challenge = value
            .get("challenge")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return Ok(WebhookAck {
            status: 200,
            body: serde_json::json!({ "challenge": challenge }).to_string(),
            event: None,
        });
    }
    let event = parse_message_event(value)?;
    Ok(WebhookAck {
        status: 200,
        body: "{\"ok\":true}".to_string(),
        event: Some(event),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_token_skips_verification() {
        assert!(verify_feishu_signature("t", "n", "b", "", "anything"));
    }

    #[test]
    fn signature_round_trips() {
        let token = "tok";
        let ts = "1700000000";
        let nonce = "abc";
        let body = r#"{"a":1}"#;
        let mut h = Sha256::new();
        h.update(ts.as_bytes());
        h.update(nonce.as_bytes());
        h.update(body.as_bytes());
        h.update(token.as_bytes());
        let sig = hex::encode(h.finalize());
        assert!(verify_feishu_signature(ts, nonce, body, token, &sig));
        assert!(!verify_feishu_signature(ts, nonce, body, token, "deadbeef"));
    }
}

// ─── Feishu adapter ──────────────────────────────────────────────────────────

/// The Feishu platform adapter: a [`MessagePlatform`] (send) + a
/// [`WebhookHandler`] (parse inbound). Construct with [`FeishuConfig`].
pub struct FeishuPlatform {
    cfg: FeishuConfig,
    http: reqwest::Client,
    token_cache: RwLock<Option<CachedToken>>,
}

struct CachedToken {
    token: String,
    fetched_at: Instant,
    /// Feishu returns `expire` in seconds; refresh early.
    ttl: Duration,
}

impl FeishuPlatform {
    pub fn new(cfg: FeishuConfig) -> Self {
        Self {
            cfg,
            http: reqwest::Client::new(),
            token_cache: RwLock::new(None),
        }
    }

    pub fn arc(cfg: FeishuConfig) -> Arc<Self> {
        Arc::new(Self::new(cfg))
    }

    /// Credentials + HTTP client, handed to the long-connection transport.
    pub fn cfg_and_http(&self) -> (FeishuConfig, reqwest::Client) {
        (self.cfg.clone(), self.http.clone())
    }

    const TOKEN_TOLERANCE: Duration = Duration::from_secs(60);

    async fn tenant_access_token(&self) -> Result<String> {
        {
            let cache = self.token_cache.read().await;
            if let Some(c) = cache.as_ref() {
                if c.fetched_at.elapsed() + Self::TOKEN_TOLERANCE < c.ttl {
                    return Ok(c.token.clone());
                }
            }
        }
        let resp = self
            .http
            .post(format!(
                "{}/open-apis/auth/v3/tenant_access_token/internal",
                self.cfg.base_url
            ))
            .json(&serde_json::json!({
                "app_id": self.cfg.app_id,
                "app_secret": self.cfg.app_secret,
            }))
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;
        let token = resp
            .get("tenant_access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| GatewayError::Platform {
                platform: "feishu".into(),
                message: "no tenant_access_token in response".into(),
            })?
            .to_string();
        let expire = resp.get("expire").and_then(|v| v.as_u64()).unwrap_or(7200);
        let mut cache = self.token_cache.write().await;
        *cache = Some(CachedToken {
            token: token.clone(),
            fetched_at: Instant::now(),
            ttl: Duration::from_secs(expire),
        });
        Ok(token)
    }
}

#[async_trait]
impl MessagePlatform for FeishuPlatform {
    fn name(&self) -> &str {
        "feishu"
    }

    fn supports_inbound_push(&self) -> bool {
        true
    }

    /// Feishu text message ~3072 chars; be conservative.
    fn max_text_length(&self) -> usize {
        3000
    }

    /// Feishu's REST send accepts repeated calls — stream the reply so the
    /// user sees incremental progress (§3.1 tail #3).
    fn supports_streaming_reply(&self) -> bool {
        true
    }

    async fn send(&self, channel: &ChannelId, text: &str) -> Result<()> {
        let token = self.tenant_access_token().await?;
        let body = serde_json::json!({
            "receive_id": channel.raw,
            "msg_type": "text",
            "content": serde_json::to_string(&serde_json::json!({ "text": text }))?,
        });
        let resp = self
            .http
            .post(format!(
                "{}/open-apis/im/v1/messages?receive_id_type=chat_id",
                self.cfg.base_url
            ))
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let txt = resp.text().await.unwrap_or_default();
            return Err(GatewayError::Platform {
                platform: "feishu".into(),
                message: format!("send failed: {status} {txt}"),
            });
        }
        Ok(())
    }
}

#[async_trait]
impl WebhookHandler for FeishuPlatform {
    fn platform(&self) -> &str {
        "feishu"
    }

    async fn parse(
        &self,
        headers: &axum::http::HeaderMap,
        body: &[u8],
        _query: &str,
    ) -> Result<WebhookAck> {
        let body_str = std::str::from_utf8(body)
            .map_err(|e| GatewayError::Parse(format!("non-utf8 body: {e}")))?;
        let value: serde_json::Value = serde_json::from_str(body_str)?;

        // Signature verification over the raw body string — applies to both
        // plaintext events and the `{"encrypt":"..."}` envelope (Feishu signs
        // the body string it sent). Skipped when no verification_token is set
        // (local dev only).
        if !self.cfg.verification_token.is_empty() {
            let ts = header_str(headers, "x-lark-request-timestamp").unwrap_or_default();
            let nonce = header_str(headers, "x-lark-request-nonce").unwrap_or_default();
            let sig = header_str(headers, "x-lark-signature").unwrap_or_default();
            if !verify_feishu_signature(&ts, &nonce, body_str, &self.cfg.verification_token, &sig) {
                return Err(GatewayError::Platform {
                    platform: "feishu".into(),
                    message: "signature verification failed".into(),
                });
            }
        }

        // Encrypted envelope (encrypt_key configured) → decrypt, then handle
        // the inner envelope (which may itself be a url_verification challenge).
        if let Some(enc) = value.get("encrypt").and_then(|v| v.as_str()) {
            let key = self.cfg.encrypt_key.as_ref().ok_or_else(|| GatewayError::Platform {
                platform: "feishu".into(),
                message:
                    "encrypted event received but FEISHU_ENCRYPT_KEY unset — set it or disable encrypt_key in Feishu backend"
                        .into(),
            })?;
            let plaintext = decrypt_event(enc, key)?;
            let inner: serde_json::Value = serde_json::from_str(&plaintext)
                .map_err(|e| GatewayError::Parse(format!("decrypted payload not JSON: {e}")))?;
            return ack_envelope(&inner);
        }

        // Plaintext event envelope.
        ack_envelope(&value)
    }
}

/// Parse an `im.message.receive_v1` event into a [`MessageEvent`].
pub(crate) fn parse_message_event(value: &serde_json::Value) -> Result<MessageEvent> {
    // Envelope: {"schema":"2.0","header":{...,"event_type":"im.message.receive_v1"},"event":{...}}
    let header = value
        .get("header")
        .ok_or_else(|| GatewayError::Parse("missing event header".into()))?;
    let event_type = header
        .get("event_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if event_type != "im.message.receive_v1" {
        return Err(GatewayError::Parse(format!(
            "unsupported feishu event_type: {event_type}"
        )));
    }
    let event = value
        .get("event")
        .ok_or_else(|| GatewayError::Parse("missing event body".into()))?;
    let message = event
        .get("message")
        .ok_or_else(|| GatewayError::Parse("missing message".into()))?;
    let chat_id = message
        .get("chat_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| GatewayError::Parse("missing chat_id".into()))?
        .to_string();
    let sender_id = event
        .get("sender")
        .and_then(|s| s.get("sender_id"))
        .and_then(|s| s.get("open_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // message.content is a JSON string like {"text":"hello"}.
    let text = message
        .get("content")
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| {
            v.get("text")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default();
    // Strip the @bot mention prefix Feishu inserts in group chats, e.g.
    // `@_user_1 hello world` → `hello world`. Removes a leading `@<token>` +
    // following whitespace. A user's own leading `@mention` is also stripped
    // (acceptable noise for a bot context).
    let text = strip_leading_mention(&text);
    Ok(MessageEvent {
        channel: ChannelId::new("feishu", chat_id),
        sender: Sender {
            id: sender_id.clone(),
            name: None,
        },
        text,
        raw: value.clone(),
        reply_to: message
            .get("message_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    })
}

fn header_str(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Strip a leading `@token` mention (e.g. `@_user_1 `) from message text.
fn strip_leading_mention(text: &str) -> String {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('@') {
        return text.to_string();
    }
    // Skip the `@` + the first non-whitespace run + following whitespace.
    let after_at = &trimmed[1..];
    match after_at.find(char::is_whitespace) {
        Some(i) => after_at[i..].trim_start().to_string(),
        None => String::new(), // only the mention, no real text
    }
}

#[cfg(test)]
mod envelope_tests {
    use super::*;

    #[test]
    fn parse_text_message() {
        let v = serde_json::json!({
            "schema": "2.0",
            "header": {"event_type": "im.message.receive_v1"},
            "event": {
                "sender": {"sender_id": {"open_id": "ou_x"}},
                "message": {
                    "chat_id": "oc_x",
                    "message_id": "om_x",
                    "content": "{\"text\":\"@_user_1 hello world\"}"
                }
            }
        });
        let ev = parse_message_event(&v).unwrap();
        assert_eq!(ev.channel.raw, "oc_x");
        assert_eq!(ev.sender.id, "ou_x");
        assert_eq!(ev.text, "hello world"); // @mention stripped
        assert_eq!(ev.reply_to.as_deref(), Some("om_x"));
    }

    #[test]
    fn reject_non_message_event() {
        let v = serde_json::json!({"header": {"event_type": "other"}, "event": {}});
        assert!(parse_message_event(&v).is_err());
    }
}

#[cfg(test)]
mod encrypt_tests {
    use super::*;
    use base64::Engine;

    /// Mirror of `decrypt_event`'s algorithm, encrypting direction — so a
    /// round-trip proves the CBC + PKCS7 implementation is self-consistent.
    fn encrypt_event(plaintext: &str, encrypt_key: &str) -> String {
        use aes::cipher::generic_array::GenericArray;
        use aes::cipher::{BlockEncrypt, KeyInit};
        use aes::Aes256;
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();
        hasher.update(encrypt_key.as_bytes());
        let key = hasher.finalize();
        let cipher = Aes256::new(GenericArray::from_slice(&key));

        // PKCS7 pad to a multiple of 16.
        let mut plain = plaintext.as_bytes().to_vec();
        let pad = 16 - (plain.len() % 16);
        plain.resize(plain.len() + pad, pad as u8);

        // Fixed IV (deterministic for the round-trip test).
        let iv = [0u8; 16];
        let mut out = Vec::with_capacity(16 + plain.len());
        out.extend_from_slice(&iv);
        let mut prev: Vec<u8> = iv.to_vec();
        for block in plain.chunks(16) {
            let mut buf = GenericArray::clone_from_slice(block);
            for (b, p) in buf.iter_mut().zip(prev.iter()) {
                *b ^= p;
            }
            cipher.encrypt_block(&mut buf);
            out.extend_from_slice(&buf);
            prev = buf.to_vec();
        }
        base64::engine::general_purpose::STANDARD.encode(&out)
    }

    #[test]
    fn decrypt_event_round_trips() {
        let key = "test_encrypt_key";
        let plaintext = r#"{"schema":"2.0","header":{"event_type":"im.message.receive_v1"},"event":{"sender":{"sender_id":{"open_id":"ou_x"}},"message":{"chat_id":"oc_x","message_id":"om_x","content":"{\"text\":\"hi\"}"}}}"#;
        let enc = encrypt_event(plaintext, key);
        let dec = decrypt_event(&enc, key).unwrap();
        assert_eq!(dec, plaintext);
    }

    #[test]
    fn decrypt_event_rejects_bad_payload() {
        assert!(decrypt_event("not-base64!!!", "k").is_err());
        // too short (< 32 bytes / not block-aligned)
        let short = base64::engine::general_purpose::STANDARD.encode(b"short");
        assert!(decrypt_event(&short, "k").is_err());
    }

    #[tokio::test]
    async fn parse_handles_encrypted_envelope() {
        // Build an FeishuPlatform with encrypt_key set; feed it an encrypted
        // im.message.receive_v1 envelope; verify the event is extracted.
        let key = "ek";
        let inner = serde_json::json!({
            "schema": "2.0",
            "header": {"event_type": "im.message.receive_v1"},
            "event": {
                "sender": {"sender_id": {"open_id": "ou_x"}},
                "message": {
                    "chat_id": "oc_enc",
                    "message_id": "om_enc",
                    "content": "{\"text\":\"hi\"}"
                }
            }
        });
        let enc = encrypt_event(&inner.to_string(), key);
        let body = serde_json::json!({ "encrypt": enc }).to_string();
        let cfg = FeishuConfig {
            app_id: "a".into(),
            app_secret: "s".into(),
            verification_token: String::new(), // skip signature
            encrypt_key: Some(key.into()),
            base_url: "https://open.feishu.cn".into(),
        };
        let plat = FeishuPlatform::new(cfg);
        let ack = plat
            .parse(&axum::http::HeaderMap::new(), body.as_bytes(), "")
            .await
            .unwrap();
        let event = ack.event.unwrap();
        assert_eq!(event.channel.raw, "oc_enc");
        assert_eq!(event.text, "hi");
    }
}
