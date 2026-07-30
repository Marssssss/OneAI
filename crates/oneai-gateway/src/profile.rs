//! Profile routing — map (platform / guild / channel / thread) → DomainPack name.
//!
//! First-cut: a table of explicit [`RouteEntry`] matches scored by specificity
//! (channel > guild > platform), with a default fallback. Whole-pack
//! *switching* at runtime is a documented follow-up (the `App`'s
//! `MergedDomainPack` is fixed at build time; per-channel packs would need one
//! `App` per pack — see evolution-plan §3.1). For now the resolved pack name
//! is logged and the gateway uses the single configured pack; the table
//! exists so future per-pack routing is a wiring change, not an API change.

use serde::{Deserialize, Serialize};

use crate::event::ChannelId;

/// One routing rule: match a platform/channel and resolve to a pack name.
///
/// Fields are matched by descending specificity. `channel` (the platform-native
/// id) is most specific; `platform` least. An empty/None field = wildcard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteEntry {
    /// Platform name to match, or any platform if `None`.
    pub platform: Option<String>,
    /// Platform-specific guild/server id (Discord guild, Slack workspace),
    /// or `None` to wildcard.
    pub guild: Option<String>,
    /// Channel id to match, or `None` to wildcard.
    pub channel: Option<String>,
    /// Thread id to match, or `None` to wildcard.
    pub thread: Option<String>,
    /// The DomainPack name to resolve to when this entry matches.
    pub pack: String,
}

impl RouteEntry {
    /// Specificity score: count of non-wildcard fields. Higher = more specific.
    fn specificity(&self) -> usize {
        [
            self.platform.is_some(),
            self.guild.is_some(),
            self.channel.is_some(),
            self.thread.is_some(),
        ]
        .iter()
        .filter(|&&b| b)
        .count()
    }

    /// Whether this entry matches the given coordinates.
    fn matches(&self, platform: &str, channel: &ChannelId) -> bool {
        if let Some(p) = &self.platform {
            if p != platform {
                return false;
            }
        }
        if let Some(c) = &self.channel {
            if c != &channel.raw {
                return false;
            }
        }
        // guild / thread are not carried on ChannelId yet; treated as wildcard
        // (matched by None) until a richer event carries them.
        true
    }
}

/// The profile router. Resolves a pack name for inbound coordinates.
#[derive(Debug, Clone, Default)]
pub struct ProfileRoute {
    entries: Vec<RouteEntry>,
    /// Fallback pack name when no entry matches.
    default_pack: String,
}

impl ProfileRoute {
    pub fn new(default_pack: impl Into<String>) -> Self {
        Self {
            entries: Vec::new(),
            default_pack: default_pack.into(),
        }
    }

    /// Add an explicit route. Higher-specificity entries win on ties; insertion
    /// order is the tiebreak.
    pub fn add(&mut self, entry: RouteEntry) {
        self.entries.push(entry);
        // Sort descending by specificity so the first match in `resolve` is the
        // most specific.
        self.entries
            .sort_by_key(|b| std::cmp::Reverse(b.specificity()));
    }

    /// Builder-style add.
    pub fn with(mut self, entry: RouteEntry) -> Self {
        self.add(entry);
        self
    }

    /// Resolve the pack name for `channel`. Returns the default when no entry
    /// matches (or when the best match's pack equals the default).
    pub fn resolve(&self, channel: &ChannelId) -> String {
        for e in &self.entries {
            if e.matches(&channel.platform, channel) {
                return e.pack.clone();
            }
        }
        self.default_pack.clone()
    }

    pub fn default_pack(&self) -> &str {
        &self.default_pack
    }

    pub fn entries(&self) -> &[RouteEntry] {
        &self.entries
    }
}
