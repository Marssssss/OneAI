//! Channel directory — the durable map from platform channel → OneAI session id.
//!
//! Mirrors `oneai-supervisor`'s `InstanceRegistry` persistence pattern: a JSON
//! file (`channels.json`) under a root dir (default `~/.oneai/gateway/`),
//! atomic write via tmp+rename, loaded on startup. The first time a channel
//! is seen, [`ChannelDirectory::resolve_or_mint`] mints a UUID session id,
//! persists it, and returns it; subsequent visits resolve to the same id — so
//! a follow-up message on the same Feishu chat / WeChat openid resumes the
//! bound OneAI session (its conversation history is reloaded by
//! `App::create_session_with_id` on the CLI side).

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::error::Result;
use crate::event::ChannelId;

/// One bound channel → session record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelBinding {
    pub platform: String,
    pub channel: String,
    /// The platform-native sender (user) id for the most recent message, if known.
    pub user_id: Option<String>,
    /// The bound OneAI session id.
    pub session_id: String,
    /// The DomainPack name this channel is bound to (§3.1 tail #1). Locked at
    /// first mint — subsequent routing changes don't migrate the session.
    #[serde(default)]
    pub pack: String,
    pub created_at: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

/// The on-disk file shape.
#[derive(Debug, Default, Serialize, Deserialize)]
struct DirectoryFile {
    channels: Vec<ChannelBinding>,
}

/// A persisted channel→session directory.
pub struct ChannelDirectory {
    root: PathBuf,
    inner: RwLock<HashMap<String, ChannelBinding>>,
}

impl ChannelDirectory {
    /// Create or load a directory rooted at `root` (`channels.json` lives
    /// directly under it). Existing bindings are loaded.
    pub async fn new(root: PathBuf) -> Result<Self> {
        let inner = RwLock::new(HashMap::new());
        let dir = Self { root, inner };
        dir.load_into().await?;
        Ok(dir)
    }

    /// Default root: `~/.oneai/gateway/`.
    pub async fn default_root() -> Result<Self> {
        let root = dirs::data_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".oneai")
            .join("gateway");
        Self::new(root).await
    }

    /// Use an in-memory store (no file). For tests and ephemeral runs.
    pub fn in_memory() -> Self {
        Self {
            root: PathBuf::new(),
            inner: RwLock::new(HashMap::new()),
        }
    }

    fn path(&self) -> PathBuf {
        self.root.join("channels.json")
    }

    fn tmp_path(&self) -> PathBuf {
        self.root.join("channels.json.tmp")
    }

    async fn load_into(&self) -> Result<()> {
        if self.root.as_os_str().is_empty() {
            return Ok(());
        }
        match tokio::fs::read_to_string(self.path()).await {
            Ok(s) => {
                let file: DirectoryFile = serde_json::from_str(&s)?;
                let mut map = self.inner.write().await;
                for b in file.channels {
                    map.insert(channel_key(&b.platform, &b.channel), b);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    async fn persist(&self) -> Result<()> {
        if self.root.as_os_str().is_empty() {
            return Ok(());
        }
        tokio::fs::create_dir_all(&self.root).await?;
        let snapshot: Vec<ChannelBinding> = self.inner.read().await.values().cloned().collect();
        let file = DirectoryFile { channels: snapshot };
        let json = serde_json::to_vec_pretty(&file)?;
        let tmp = self.tmp_path();
        tokio::fs::write(&tmp, &json).await?;
        tokio::fs::rename(&tmp, self.path()).await?;
        Ok(())
    }

    /// Resolve the bound session id for `channel`, minting + persisting a new
    /// UUID session id on first contact. Idempotent on subsequent visits.
    ///
    /// `pack` is the DomainPack name resolved for this channel. It is **locked
    /// at first mint** — written to the binding once and left untouched on
    /// later visits, so a routing-rule change doesn't yank an existing
    /// channel's session into a different pack (and lose its conversation
    /// history). Callers that want the *effective* pack should read it back
    /// via [`ChannelDirectory::get`].
    pub async fn resolve_or_mint(
        &self,
        channel: &ChannelId,
        user_id: Option<&str>,
        pack: &str,
    ) -> Result<String> {
        let key = channel.key();
        let now = Utc::now();
        let mut map = self.inner.write().await;
        if let Some(b) = map.get_mut(&key) {
            if let Some(uid) = user_id {
                b.user_id = Some(uid.to_string());
            }
            b.last_seen = now;
            let session_id = b.session_id.clone();
            drop(map);
            self.persist().await?;
            return Ok(session_id);
        }
        let session_id = Uuid::new_v4().to_string();
        let binding = ChannelBinding {
            platform: channel.platform.clone(),
            channel: channel.raw.clone(),
            user_id: user_id.map(|s| s.to_string()),
            session_id: session_id.clone(),
            pack: pack.to_string(),
            created_at: now,
            last_seen: now,
        };
        map.insert(key, binding);
        drop(map);
        self.persist().await?;
        Ok(session_id)
    }

    /// Look up a binding without minting.
    pub async fn get(&self, channel: &ChannelId) -> Option<ChannelBinding> {
        self.inner.read().await.get(&channel.key()).cloned()
    }

    /// All bindings (for the CLI `gateway channels` listing).
    pub async fn list(&self) -> Vec<ChannelBinding> {
        self.inner.read().await.values().cloned().collect()
    }

    /// Drop a binding (e.g. admin reset). No-op if absent.
    pub async fn forget(&self, channel: &ChannelId) -> Result<()> {
        let removed = self.inner.write().await.remove(&channel.key()).is_some();
        if removed {
            self.persist().await?;
        }
        Ok(())
    }
}

fn channel_key(platform: &str, channel: &str) -> String {
    format!("{}\u{0}{}", platform, channel)
}
