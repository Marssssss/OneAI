//! Conversation listing — the shared, front-end-agnostic handle for saved
//! sessions, surfaced over the `session/list` JSON-RPC method.
//!
//! Mirrors [`crate::scenario::ScenarioStore`] in shape: an object-safe
//! `#[async_trait]` the app-server holds as `Arc<dyn ConversationStore + Send +
//! Sync>`, so the crate stays decoupled from `oneai-app` (it never touches an
//! `App` directly — the CLI passes a concrete impl wrapping `Arc<App>`). The
//! trait returns `oneai_core::SessionInfo`; the adapter serializes each entry
//! to the epoch-millis shape the FFI `SessionInfoView` exposes, so a foreign
//! UI renders one list regardless of transport (in-process FFI or sidecar).
//!
//! `session/list` is synchronous CRUD (like `scenario/list`) — it does not go
//! through the bus/Directive path. `session/create`/`load`/`clear`/`delete`
//! remain bus-driven (their result arrives as an `EngineYield`); only listing
//! needs no engine round-trip, so it short-circuits straight to the store.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use oneai_core::SessionInfo;

/// A handle to the durable conversation store, backing `session/list`. Object-
/// safe via `#[async_trait]` so the app-server holds
/// `Arc<dyn ConversationStore + Send + Sync>` and tests can swap an in-memory
/// impl. The production impl (in `cmd_app_server`) wraps `Arc<oneai_app::App>`
/// and calls `App::list_conversations`.
#[async_trait]
pub trait ConversationStore: Send + Sync {
    /// All saved conversations, in store order (the caller re-sorts newest-
    /// first for display — same as the FFI `list_conversations` consumer).
    /// Errors are swallowed by the production impl (it `unwrap_or_default`s),
    /// so this returns the list directly; a backend that fails returns empty.
    async fn list(&self) -> Vec<SessionInfo>;

    /// Rename a conversation's title. Returns `true` when a row was updated,
    /// `false` when no saved session matches `id` (errors are swallowed — a
    /// failing backend returns `false`, mirroring `list`'s swallow contract;
    /// the adapter surfaces `false` as a not-found error to the frontend).
    /// Empty/whitespace titles are a no-op returning `true`.
    async fn rename(&self, id: &str, title: &str) -> bool;

    /// Toggle a conversation's archived flag. Returns `true` when a row was
    /// updated, `false` when not found (errors swallowed → `false`).
    async fn set_archived(&self, id: &str, archived: bool) -> bool;
}

/// Shared, thread-safe handle threaded through `serve_all` → transports →
/// `serve_connection` → `handle_request` for the `session/list` method.
/// `Arc<dyn ConversationStore + Send + Sync>`.
pub type SharedConversationStore = Arc<dyn ConversationStore + Send + Sync>;

/// An in-memory [`ConversationStore`] for tests — no IO, deterministic. Holds
/// a `Vec<SessionInfo>` under a `Mutex` so the trait is `&self` + object-safe.
pub struct InMemoryConversationStore {
    sessions: Mutex<Vec<SessionInfo>>,
}

impl Default for InMemoryConversationStore {
    fn default() -> Self {
        Self {
            sessions: Mutex::new(Vec::new()),
        }
    }
}

impl InMemoryConversationStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn from_seed(sessions: Vec<SessionInfo>) -> Self {
        Self {
            sessions: Mutex::new(sessions),
        }
    }
}

#[async_trait]
impl ConversationStore for InMemoryConversationStore {
    async fn list(&self) -> Vec<SessionInfo> {
        self.sessions.lock().await.clone()
    }
    async fn rename(&self, id: &str, title: &str) -> bool {
        let trimmed = title.trim();
        if trimmed.is_empty() {
            // No-op matches the durable impl: an empty rename is "keep current".
            return true;
        }
        let mut sessions = self.sessions.lock().await;
        for s in sessions.iter_mut() {
            if s.id == id {
                s.title = Some(trimmed.to_string());
                return true;
            }
        }
        false
    }
    async fn set_archived(&self, id: &str, archived: bool) -> bool {
        let mut sessions = self.sessions.lock().await;
        for s in sessions.iter_mut() {
            if s.id == id {
                s.archived = archived;
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn info(id: &str, count: usize) -> SessionInfo {
        let now = Utc::now();
        SessionInfo::new(id.into(), now, now, count)
    }

    #[tokio::test]
    async fn in_memory_store_returns_seed_in_order() {
        let store = InMemoryConversationStore::from_seed(vec![info("a", 1), info("b", 2)]);
        let list = store.list().await;
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "a");
        assert_eq!(list[1].id, "b");
    }

    #[tokio::test]
    async fn empty_store_returns_empty() {
        let store = InMemoryConversationStore::new();
        assert!(store.list().await.is_empty());
    }

    #[tokio::test]
    async fn rename_updates_title_and_returns_false_when_missing() {
        let store = InMemoryConversationStore::from_seed(vec![info("a", 1)]);
        assert!(store.rename("a", "New Title").await);
        assert_eq!(store.list().await[0].title.as_deref(), Some("New Title"));
        // Empty title is a no-op (keeps current), still returns true.
        assert!(store.rename("a", "  ").await);
        assert_eq!(store.list().await[0].title.as_deref(), Some("New Title"));
        // Missing id → false.
        assert!(!store.rename("nope", "x").await);
    }

    #[tokio::test]
    async fn set_archived_toggles_and_returns_false_when_missing() {
        let store = InMemoryConversationStore::from_seed(vec![info("a", 1)]);
        assert!(!store.list().await[0].archived);
        assert!(store.set_archived("a", true).await);
        assert!(store.list().await[0].archived);
        assert!(store.set_archived("a", false).await);
        assert!(!store.list().await[0].archived);
        assert!(!store.set_archived("nope", true).await);
    }
}
