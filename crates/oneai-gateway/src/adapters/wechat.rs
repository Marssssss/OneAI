//! WeChat Official Account adapter — signature handshake + XML inbound + REST send.
//!
//! ## Inbound
//! - **GET handshake** (`GET /gateway/wechat?signature=..&timestamp=..&nonce=..&echostr=..`):
//!   verify `sha1(sorted([token, timestamp, nonce]).join(""))` == `signature`,
//!   then echo back `echostr` plaintext. This is the one-time URL registration.
//! - **POST message**: XML body of an `<xml>` with `<MsgType>text</MsgType>`,
//!   `<Content>`, `<FromUserName>` (openid), `<MsgId>`. Parsed via `quick-xml`
//!   to a [`MessageEvent`] whose `channel.raw` = `FromUserName` (the sender's
//!   openid — OA replies are addressed to the *sender*, there is no chat id).
//!
//! ## Outbound
//! `access_token` (cached with expiry, `/cgi-bin/token`) → `POST
//! api.weixin.qq.com/cgi-bin/message/custom/send`.
//!
//! ## Scope
//! Plaintext mode only (the OA default for message receive). Compatible /
//! safe-mode AES-encrypted messages are a documented follow-up.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use sha1::{Digest, Sha1};
use tokio::sync::RwLock;

use crate::error::{GatewayError, Result};
use crate::event::{ChannelId, MessageEvent, Sender};
use crate::platform::MessagePlatform;
use crate::web::{WebhookAck, WebhookHandler};

/// WeChat OA config.
#[derive(Clone)]
pub struct WeChatConfig {
    pub app_id: String,
    pub app_secret: String,
    /// The "Token" configured on the OA → 基本配置 page (for handshake sig).
    pub token: String,
    pub base_url: String,
}

impl WeChatConfig {
    pub fn from_env() -> Option<Self> {
        Some(Self {
            app_id: std::env::var("WECHAT_APPID").ok()?,
            app_secret: std::env::var("WECHAT_SECRET").ok()?,
            token: std::env::var("WECHAT_TOKEN").ok().unwrap_or_default(),
            base_url: std::env::var("WECHAT_BASE_URL")
                .unwrap_or_else(|_| "https://api.weixin.qq.com".to_string()),
        })
    }
}

/// WeChat handshake signature: `sha1` of the sorted join of token+timestamp+nonce.
pub fn verify_wechat_signature(token: &str, timestamp: &str, nonce: &str, signature: &str) -> bool {
    let mut parts = [token, timestamp, nonce];
    parts.sort();
    let joined = parts.join("");
    let mut h = Sha1::new();
    h.update(joined.as_bytes());
    let computed = hex::encode(h.finalize());
    computed == signature
}

/// Parse the `?signature=&timestamp=&nonce=&echostr=` query string.
fn parse_query(query: &str) -> std::collections::HashMap<&str, &str> {
    query
        .split('&')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k, v))
        })
        .collect()
}

#[cfg(test)]
mod sig_tests {
    use super::*;

    #[test]
    fn signature_round_trips() {
        // token + timestamp + nonce, sorted.
        let token = "token";
        let ts = "1700000000";
        let nonce = "abc";
        let mut parts = [token, ts, nonce];
        parts.sort();
        let mut h = Sha1::new();
        h.update(parts.join("").as_bytes());
        let sig = hex::encode(h.finalize());
        assert!(verify_wechat_signature(token, ts, nonce, &sig));
        assert!(!verify_wechat_signature(token, ts, nonce, "deadbeef"));
    }

    #[test]
    fn query_parse() {
        let q = "signature=abc&timestamp=1&nonce=n&echostr=hi";
        let m = parse_query(q);
        assert_eq!(m.get("signature"), Some(&"abc"));
        assert_eq!(m.get("echostr"), Some(&"hi"));
    }
}

// ─── WeChat adapter ───────────────────────────────────────────────────────────

pub struct WeChatPlatform {
    cfg: WeChatConfig,
    http: reqwest::Client,
    token_cache: RwLock<Option<CachedToken>>,
}

struct CachedToken {
    token: String,
    fetched_at: Instant,
    ttl: Duration,
}

impl WeChatPlatform {
    pub fn new(cfg: WeChatConfig) -> Self {
        Self {
            cfg,
            http: reqwest::Client::new(),
            token_cache: RwLock::new(None),
        }
    }

    pub fn arc(cfg: WeChatConfig) -> Arc<Self> {
        Arc::new(Self::new(cfg))
    }

    const TOKEN_TOLERANCE: Duration = Duration::from_secs(60);

    async fn access_token(&self) -> Result<String> {
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
            .get(format!(
                "{}/cgi-bin/token?grant_type=client_credential&appid={}&secret={}",
                self.cfg.base_url, self.cfg.app_id, self.cfg.app_secret
            ))
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;
        if let Some(errcode) = resp.get("errcode").and_then(|v| v.as_i64()) {
            if errcode != 0 {
                return Err(GatewayError::Platform {
                    platform: "wechat".into(),
                    message: format!("token err: {resp}"),
                });
            }
        }
        let token = resp
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| GatewayError::Platform {
                platform: "wechat".into(),
                message: "no access_token in response".into(),
            })?
            .to_string();
        let expire = resp
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(7200);
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
impl MessagePlatform for WeChatPlatform {
    fn name(&self) -> &str {
        "wechat"
    }

    fn supports_inbound_push(&self) -> bool {
        true
    }

    /// WeChat OA customer-service text messages are capped at ~600 chars.
    fn max_text_length(&self) -> usize {
        500
    }

    async fn send(&self, channel: &ChannelId, text: &str) -> Result<()> {
        let token = self.access_token().await?;
        let body = serde_json::json!({
            "touser": channel.raw,
            "msgtype": "text",
            "text": { "content": text },
        });
        let resp = self
            .http
            .post(format!(
                "{}/cgi-bin/message/custom/send?access_token={}",
                self.cfg.base_url, token
            ))
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let txt = resp.text().await.unwrap_or_default();
            return Err(GatewayError::Platform {
                platform: "wechat".into(),
                message: format!("send failed: {status} {txt}"),
            });
        }
        Ok(())
    }
}

#[async_trait]
impl WebhookHandler for WeChatPlatform {
    fn platform(&self) -> &str {
        "wechat"
    }

    async fn parse(
        &self,
        _headers: &axum::http::HeaderMap,
        body: &[u8],
        query: &str,
    ) -> Result<WebhookAck> {
        // GET handshake: body empty, query carries signature/timestamp/nonce/echostr.
        if body.is_empty() {
            let q = parse_query(query);
            let signature = q.get("signature").copied().unwrap_or("");
            let timestamp = q.get("timestamp").copied().unwrap_or("");
            let nonce = q.get("nonce").copied().unwrap_or("");
            let echostr = q.get("echostr").copied().unwrap_or("");
            if !self.cfg.token.is_empty()
                && !verify_wechat_signature(&self.cfg.token, timestamp, nonce, signature)
            {
                return Err(GatewayError::Platform {
                    platform: "wechat".into(),
                    message: "handshake signature mismatch".into(),
                });
            }
            return Ok(WebhookAck {
                status: 200,
                body: echostr.to_string(),
                event: None,
            });
        }

        // POST message — XML body.
        let xml_str = std::str::from_utf8(body)
            .map_err(|e| GatewayError::Parse(format!("non-utf8 xml: {e}")))?;
        let event = parse_wechat_xml(xml_str)?;
        Ok(WebhookAck {
            status: 200,
            body: "success".to_string(),
            event: Some(event),
        })
    }
}

/// Parse a WeChat OA message XML.
fn parse_wechat_xml(xml: &str) -> Result<MessageEvent> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut current_tag: Option<String> = None;
    let mut fields: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                current_tag = Some(
                    std::str::from_utf8(e.name().as_ref())
                        .unwrap_or("")
                        .to_string(),
                );
            }
            Ok(Event::End(_)) => {
                current_tag = None;
            }
            Ok(Event::Text(t)) => {
                if let Some(tag) = &current_tag {
                    let txt = String::from_utf8_lossy(t.as_ref()).to_string();
                    fields.entry(tag.clone()).or_default().push_str(&txt);
                }
            }
            Ok(Event::CData(t)) => {
                if let Some(tag) = &current_tag {
                    let txt = String::from_utf8_lossy(t.as_ref()).to_string();
                    fields.entry(tag.clone()).or_default().push_str(&txt);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(GatewayError::Parse(format!("xml parse: {e}")));
            }
            _ => {}
        }
        buf.clear();
    }

    let msg_type = fields.get("MsgType").map(|s| s.as_str()).unwrap_or("");
    if msg_type != "text" {
        return Err(GatewayError::Parse(format!(
            "unsupported wechat MsgType: {msg_type}"
        )));
    }
    let from_user = fields.get("FromUserName").cloned().unwrap_or_default();
    let content = fields.get("Content").cloned().unwrap_or_default();
    let msg_id = fields.get("MsgId").cloned();
    Ok(MessageEvent {
        channel: ChannelId::new("wechat", from_user.clone()),
        sender: Sender::anonymous(from_user),
        text: content.trim().to_string(),
        raw: serde_json::to_value(&fields).unwrap_or(serde_json::Value::Null),
        reply_to: msg_id,
    })
}

#[cfg(test)]
mod xml_tests {
    use super::*;

    #[test]
    fn parse_text_message_xml() {
        let xml = r#"<xml>
            <ToUserName><![CDATA[gh_x]]></ToUserName>
            <FromUserName><![CDATA[oABC]]></FromUserName>
            <CreateTime>1700000000</CreateTime>
            <MsgType><![CDATA[text]]></MsgType>
            <Content><![CDATA[hello 微信]]></Content>
            <MsgId>1234</MsgId>
        </xml>"#;
        let ev = parse_wechat_xml(xml).unwrap();
        assert_eq!(ev.channel.raw, "oABC");
        assert_eq!(ev.text, "hello 微信");
        assert_eq!(ev.reply_to.as_deref(), Some("1234"));
    }

    #[test]
    fn reject_non_text() {
        let xml = r#"<xml><MsgType><![CDATA[image]]></MsgType></xml>"#;
        assert!(parse_wechat_xml(xml).is_err());
    }
}
