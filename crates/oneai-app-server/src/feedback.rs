//! Per-message feedback — the shared, front-end-agnostic handle for the
//! user's 👍/👎/note reaction to a specific assistant message, surfaced over
//! the `feedback/submit` + `feedback/list` JSON-RPC methods.
//!
//! Mirrors [`crate::conversation::ConversationStore`] in shape: an object-safe
//! `#[async_trait]` the app-server holds as `Arc<dyn FeedbackStore + Send +
//! Sync>`, so the crate stays decoupled from `oneai-app` (it never touches an
//! `App` directly — the CLI passes a concrete impl wrapping `Arc<App>`).
//!
//! `feedback/submit` / `feedback/list` are synchronous CRUD (like
//! `session/list`) — they do not go through the bus/Directive path. Feedback is
//! user→engine metadata about an already-emitted message, not a turn action;
//! the engine does not re-infer on it (closed-loop use is future work).

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::sync::Mutex;

// `FeedbackEntry` is defined in `oneai-core` (shared with the persistence
// layer); re-export here so `lib.rs` can `pub use feedback::{FeedbackEntry, …}`
// and so this module can name the type directly.
pub use oneai_core::FeedbackEntry;

/// Epoch-millis timestamp via `SystemTime` — no `chrono` dep needed at this
/// layer (the adapter already serializes other timestamps as epoch-ms).
fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The kind of per-message feedback a user can record.
///
/// `up` / `down` are the 👍/👎 quick reactions; `note` carries free-text
/// commentary in [`FeedbackEntry::text`].
pub const KIND_UP: &str = "up";
pub const KIND_DOWN: &str = "down";
pub const KIND_NOTE: &str = "note";

/// A handle to the durable feedback store, backing `feedback/submit` +
/// `feedback/list`. Object-safe via `#[async_trait]` so the app-server holds
/// `Arc<dyn FeedbackStore + Send + Sync>` and tests can swap an in-memory
/// impl. The production impl (in `cmd_app_server`) wraps `Arc<oneai_app::App>`
/// and delegates to `App::record_feedback` / `App::list_feedback` (which in
/// turn hit the shared SQLite store).
#[async_trait]
pub trait FeedbackStore: Send + Sync {
    /// Record one feedback entry. `text` is `Some` only for `KIND_NOTE`. The
    /// store assigns `id` + `created_at_ms`; errors are swallowed by the
    /// production impl (it `unwrap_or_default`s — a failing backend surfaces
    /// as a silent no-op, never a panic), so this returns nothing on success.
    async fn record(
        &self,
        session_id: &str,
        turn_id: &str,
        message_role: &str,
        kind: &str,
        text: Option<&str>,
    );

    /// All feedback entries for `session_id`, in store order (the caller
    /// re-sorts newest-first for display — same as the `session/list`
    /// consumer). A backend that fails returns empty.
    async fn list(&self, session_id: &str) -> Vec<FeedbackEntry>;
}

/// Shared, thread-safe handle threaded through `serve_all` → transports →
/// `serve_connection` → `handle_request` for the `feedback/*` methods.
/// `Arc<dyn FeedbackStore + Send + Sync>`.
pub type SharedFeedbackStore = Arc<dyn FeedbackStore + Send + Sync>;

/// An in-memory [`FeedbackStore`] for tests — no IO, deterministic. Holds a
/// `Vec<FeedbackEntry>` under a `Mutex` so the trait is `&self` + object-safe.
pub struct InMemoryFeedbackStore {
    entries: Mutex<Vec<FeedbackEntry>>,
    next_id: Mutex<u64>,
}

impl Default for InMemoryFeedbackStore {
    fn default() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            next_id: Mutex::new(1),
        }
    }
}

impl InMemoryFeedbackStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn from_seed(entries: Vec<FeedbackEntry>) -> Self {
        let next = entries
            .iter()
            .filter_map(|e| e.id.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        Self {
            entries: Mutex::new(entries),
            next_id: Mutex::new(next + 1),
        }
    }
}

#[async_trait]
impl FeedbackStore for InMemoryFeedbackStore {
    async fn record(
        &self,
        session_id: &str,
        turn_id: &str,
        message_role: &str,
        kind: &str,
        text: Option<&str>,
    ) {
        let mut id_guard = self.next_id.lock().await;
        let id = format!("fb-{}", *id_guard);
        *id_guard += 1;
        drop(id_guard);
        let created_at_ms = epoch_ms();
        self.entries.lock().await.push(FeedbackEntry {
            id,
            session_id: session_id.to_string(),
            turn_id: turn_id.to_string(),
            message_role: message_role.to_string(),
            kind: kind.to_string(),
            text: text.map(|s| s.to_string()),
            created_at_ms,
        });
    }

    async fn list(&self, session_id: &str) -> Vec<FeedbackEntry> {
        self.entries
            .lock()
            .await
            .iter()
            .filter(|e| e.session_id == session_id)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn record_then_list_round_trips() {
        let store = InMemoryFeedbackStore::new();
        store.record("s1", "t1", "assistant", KIND_UP, None).await;
        store
            .record("s1", "t2", "assistant", KIND_NOTE, Some("great"))
            .await;
        store.record("s2", "t9", "assistant", KIND_DOWN, None).await;
        let list = store.list("s1").await;
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|e| e.turn_id == "t1" && e.kind == KIND_UP));
        assert!(list
            .iter()
            .any(|e| e.turn_id == "t2" && e.text.as_deref() == Some("great")));
        assert_eq!(store.list("s2").await.len(), 1);
        assert!(store.list("s3").await.is_empty());
    }

    #[tokio::test]
    async fn record_assigns_unique_ids_and_timestamps() {
        let store = InMemoryFeedbackStore::new();
        store.record("s", "t1", "assistant", KIND_UP, None).await;
        store.record("s", "t2", "assistant", KIND_UP, None).await;
        let list = store.list("s").await;
        assert_ne!(list[0].id, list[1].id);
        assert!(list[0].created_at_ms > 0 && list[1].created_at_ms > 0);
    }
}
