//! PgMemoryStore integration tests (MVS3-B storage externalization).
//!
//! Gated twice, mirroring `tests/pg_working_state.rs`:
//! 1. Compile-time: the whole file is empty unless `--features postgres`.
//! 2. Run-time: `#[ignore]` + `ONEAI_TEST_PG_DSN` — CI never needs Postgres.
//!
//! The server MUST provide pgvector (hard dependency — `ensure_schema` runs
//! `CREATE EXTENSION vector`):
//! ```text
//! docker run -d --name oneai-pg-test -p 5432:5432 \
//!   -e POSTGRES_PASSWORD=oneai -e POSTGRES_DB=oneai_test pgvector/pgvector:pg16
//! ONEAI_TEST_PG_DSN=postgres://postgres:oneai@127.0.0.1:5432/oneai_test \
//!   cargo test -p oneai-persistence --features postgres \
//!   --test pg_memory_store -- --ignored
//! ```
//!
//! Coverage mirrors the `SqliteSessionStore` unit tests (STM/LTM/conversation/
//! discarded-snapshot/facts + the pgvector KNN path that replaces the in-Rust
//! brute-force cosine) plus the multi-writer concurrency + restart-persistence
//! properties that motivate the Pg backend. Each test runs under a unique
//! scope prefix and cleans up its own rows, so a shared test DB stays tidy.
//! (`clear_ltm` is global by contract — deliberately NOT exercised here; it
//! would wipe sibling/parallel runs' rows.)

#![cfg(feature = "postgres")]

use std::collections::HashMap;

use oneai_core::traits::MemoryPersistence;
use oneai_core::{Conversation, FactType, MemoryEntry, MemoryFact, DISCARDED_SNAPSHOT_MARKER};
use oneai_persistence::PgMemoryStore;

/// Connect when `ONEAI_TEST_PG_DSN` is set; `None` = skip the test.
async fn pg_store() -> Option<PgMemoryStore> {
    let dsn = std::env::var("ONEAI_TEST_PG_DSN")
        .ok()
        .filter(|s| !s.is_empty())?;
    Some(PgMemoryStore::connect(&dsn).await.expect(
        "ONEAI_TEST_PG_DSN is set but the connection failed — is Postgres (with pgvector) running?",
    ))
}

/// A unique scope per test run (timestamp + label) so concurrent / repeated
/// runs against the same DB never see each other's rows. Used as the id /
/// session_id / user_id prefix.
fn scope(label: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("pgmem-{label}-{nanos}")
}

fn make_entry(id: &str, content: &str, embedding: Option<Vec<f32>>) -> MemoryEntry {
    MemoryEntry {
        id: id.to_string(),
        content: content.to_string(),
        timestamp: chrono::Utc::now(),
        embedding,
        metadata: HashMap::from([("role".to_string(), "user".to_string())]),
    }
}

fn make_fact(user: &str, subject: &str, predicate: &str, content: &str) -> MemoryFact {
    MemoryFact {
        id: format!("{user}-{subject}-{predicate}"),
        user_id: user.to_string(),
        session_id: "s1".to_string(),
        fact_type: FactType::new("user_tooling_pref"),
        subject: subject.to_string(),
        predicate: predicate.to_string(),
        content: content.to_string(),
        embedding: None,
        metadata: HashMap::new(),
        importance: 0.5,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        version: 1,
        superseded: false,
        superseded_at: None,
        pinned: false,
    }
}

/// Delete every row this test created (all four tables keyed by the scope
/// prefix). Stale rows from a crashed run are harmless — scopes are unique.
async fn cleanup(store: &PgMemoryStore, s: &str) {
    let client = store.pool().get().await.expect("pool client");
    let pat = format!("{s}%");
    for sql in [
        "DELETE FROM stm_entries_pg WHERE session_id LIKE $1 OR id LIKE $1",
        "DELETE FROM ltm_entries_pg WHERE id LIKE $1",
        "DELETE FROM conversations_pg WHERE id LIKE $1",
        "DELETE FROM memories_pg WHERE user_id LIKE $1",
    ] {
        client.execute(sql, &[&pat]).await.expect("cleanup");
    }
}

// ─── STM ─────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres with pgvector)"]
async fn stm_save_load_roundtrip() {
    let Some(store) = pg_store().await else {
        return;
    };
    let s = scope("stm-rt");
    let session = format!("{s}-sess");

    let entries = vec![
        make_entry(&format!("{s}-e1"), "First message", None),
        make_entry(
            &format!("{s}-e2"),
            "Second message",
            Some(vec![0.1, 0.2, 0.3]),
        ),
    ];
    store.save_stm(&session, &entries).await.unwrap();
    let loaded = store.load_stm(&session).await.unwrap();

    assert_eq!(loaded.len(), 2);
    // Ordered by position, not insertion luck.
    assert_eq!(loaded[0].content, "First message");
    assert_eq!(loaded[1].content, "Second message");
    assert!(loaded[0].embedding.is_none());
    let emb = loaded[1].embedding.as_ref().unwrap();
    assert!((emb[0] - 0.1).abs() < 1e-6 && (emb[2] - 0.3).abs() < 1e-6);
    assert_eq!(
        loaded[0].metadata.get("role").map(String::as_str),
        Some("user")
    );

    cleanup(&store, &s).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres with pgvector)"]
async fn stm_resave_replaces_set() {
    let Some(store) = pg_store().await else {
        return;
    };
    let s = scope("stm-replace");
    let session = format!("{s}-sess");

    store
        .save_stm(&session, &[make_entry(&format!("{s}-a"), "old", None)])
        .await
        .unwrap();
    store
        .save_stm(
            &session,
            &[
                make_entry(&format!("{s}-b"), "new1", None),
                make_entry(&format!("{s}-c"), "new2", None),
            ],
        )
        .await
        .unwrap();
    let loaded = store.load_stm(&session).await.unwrap();
    assert_eq!(loaded.len(), 2);
    assert!(loaded.iter().all(|e| e.content.starts_with("new")));

    cleanup(&store, &s).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres with pgvector)"]
async fn stm_clear_and_multiple_sessions() {
    let Some(store) = pg_store().await else {
        return;
    };
    let s = scope("stm-multi");
    let (s1, s2) = (format!("{s}-a"), format!("{s}-b"));

    store
        .save_stm(&s1, &[make_entry(&format!("{s}-e1"), "x", None)])
        .await
        .unwrap();
    store
        .save_stm(&s2, &[make_entry(&format!("{s}-e2"), "y", None)])
        .await
        .unwrap();
    store.clear_stm(&s1).await.unwrap();
    assert!(store.load_stm(&s1).await.unwrap().is_empty());
    assert_eq!(store.load_stm(&s2).await.unwrap().len(), 1);

    cleanup(&store, &s).await;
}

// ─── LTM ─────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres with pgvector)"]
async fn ltm_crud() {
    let Some(store) = pg_store().await else {
        return;
    };
    let s = scope("ltm-crud");
    let id = format!("{s}-entry");

    // Nonexistent → None
    assert!(store.load_ltm(&id).await.unwrap().is_none());

    store
        .save_ltm(&make_entry(&id, "knowledge", Some(vec![1.0, 0.0])))
        .await
        .unwrap();
    let loaded = store.load_ltm(&id).await.unwrap().unwrap();
    assert_eq!(loaded.content, "knowledge");
    assert_eq!(loaded.embedding.as_ref().unwrap(), &vec![1.0, 0.0]);

    // Overwrite (INSERT OR REPLACE parity)
    store
        .save_ltm(&make_entry(&id, "updated knowledge", None))
        .await
        .unwrap();
    let loaded = store.load_ltm(&id).await.unwrap().unwrap();
    assert_eq!(loaded.content, "updated knowledge");
    assert!(loaded.embedding.is_none());

    store.delete_ltm(&id).await.unwrap();
    assert!(store.load_ltm(&id).await.unwrap().is_none());

    cleanup(&store, &s).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres with pgvector)"]
async fn ltm_keyword_search_case_insensitive() {
    let Some(store) = pg_store().await else {
        return;
    };
    let s = scope("ltm-kw");
    // Mixed case in the stored content: SQLite LIKE is case-insensitive for
    // ASCII; the Pg backend must match via ILIKE.
    store
        .save_ltm(&make_entry(
            &format!("{s}-1"),
            &format!("{s} Rust FRAMEWORK notes"),
            None,
        ))
        .await
        .unwrap();
    store
        .save_ltm(&make_entry(&format!("{s}-2"), "unrelated content", None))
        .await
        .unwrap();

    let hits = store
        .search_ltm_keyword("rust framework", 10)
        .await
        .unwrap();
    let scoped: Vec<_> = hits.iter().filter(|e| e.id.starts_with(&s)).collect();
    assert_eq!(scoped.len(), 1, "ILIKE must match case-insensitively");

    // top_k is honored (a global keyword could match sibling rows; scope keeps
    // the assertion deterministic).
    let hits = store.search_ltm_keyword(&s, 1).await.unwrap();
    assert_eq!(hits.len(), 1);

    cleanup(&store, &s).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres with pgvector)"]
async fn ltm_embedding_search_pgvector_knn() {
    let Some(store) = pg_store().await else {
        return;
    };
    let s = scope("ltm-emb");

    // Identical direction → similarity ≈ 1.0 (top hit).
    store
        .save_ltm(&make_entry(
            &format!("{s}-same"),
            "same direction",
            Some(vec![1.0, 0.0, 0.0]),
        ))
        .await
        .unwrap();
    // Partial overlap → 0 < similarity < 1.
    store
        .save_ltm(&make_entry(
            &format!("{s}-mid"),
            "mid similarity",
            Some(vec![1.0, 1.0, 0.0]),
        ))
        .await
        .unwrap();
    // Orthogonal → similarity 0 → filtered (score > 0 parity with SQLite).
    store
        .save_ltm(&make_entry(
            &format!("{s}-ortho"),
            "orthogonal",
            Some(vec![0.0, 0.0, 1.0]),
        ))
        .await
        .unwrap();
    // Negative cosine → filtered.
    store
        .save_ltm(&make_entry(
            &format!("{s}-neg"),
            "opposite",
            Some(vec![-1.0, 0.0, 0.0]),
        ))
        .await
        .unwrap();
    // Different-dim embedding (another model) → never evaluated by `<=>`
    // (dim-mismatch would error), never returned.
    store
        .save_ltm(&make_entry(
            &format!("{s}-otherdim"),
            "other model",
            Some(vec![1.0, 0.0, 0.0, 0.0, 0.0]),
        ))
        .await
        .unwrap();
    // No embedding → excluded.
    store
        .save_ltm(&make_entry(&format!("{s}-noemb"), "plain", None))
        .await
        .unwrap();

    let results = store
        .search_ltm_embedding(&[1.0, 0.0, 0.0], 10)
        .await
        .unwrap();
    let scoped: Vec<_> = results
        .iter()
        .filter(|(e, _)| e.id.starts_with(&s))
        .collect();
    assert_eq!(scoped.len(), 2, "only positive-similarity same-dim rows");
    assert!(scoped[0].0.id.ends_with("-same"));
    assert!((scoped[0].1 - 1.0).abs() < 1e-5);
    assert!(scoped[1].0.id.ends_with("-mid"));
    // cos([1,0,0],[1,1,0]) = 1/√2
    assert!((scoped[1].1 - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-5);
    // Results arrive similarity-desc.
    assert!(scoped[0].1 >= scoped[1].1);
    // The entry embedding is returned populated (SQLite parity).
    assert_eq!(
        scoped[0].0.embedding.as_ref().unwrap(),
        &vec![1.0, 0.0, 0.0]
    );

    // top_k truncation.
    let top1 = store
        .search_ltm_embedding(&[1.0, 0.0, 0.0], 1)
        .await
        .unwrap();
    assert!(top1.iter().filter(|(e, _)| e.id.starts_with(&s)).count() <= 1);

    // Empty query → empty (pgvector rejects empty vectors; SQLite's cosine
    // returns 0.0 → no results).
    assert!(store
        .search_ltm_embedding(&[], 10)
        .await
        .unwrap()
        .is_empty());

    cleanup(&store, &s).await;
}

// ─── Conversations ───────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres with pgvector)"]
async fn conversation_save_load_update() {
    let Some(store) = pg_store().await else {
        return;
    };
    let s = scope("conv-rt");

    assert!(store.load_conversation(&s).await.unwrap().is_none());

    let mut conv = Conversation::with_id(s.clone());
    conv.add_message(oneai_core::Message::user("Hello".to_string()));
    conv.add_message(oneai_core::Message::assistant("Hi there".to_string()));
    conv.metadata
        .insert("workspace".to_string(), "/tmp/ws".to_string());
    store.save_conversation(&s, &conv).await.unwrap();

    let loaded = store.load_conversation(&s).await.unwrap().unwrap();
    assert_eq!(loaded.messages.len(), 2);
    assert_eq!(loaded.messages[0].text_content(), "Hello");
    assert_eq!(
        loaded.metadata.get("workspace").map(String::as_str),
        Some("/tmp/ws")
    );

    // Update: append + resave.
    conv.add_message(oneai_core::Message::user("More".to_string()));
    store.save_conversation(&s, &conv).await.unwrap();
    let loaded = store.load_conversation(&s).await.unwrap().unwrap();
    assert_eq!(loaded.messages.len(), 3);

    cleanup(&store, &s).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres with pgvector)"]
async fn conversation_title_derivation_and_rename_merge() {
    let Some(store) = pg_store().await else {
        return;
    };
    let s = scope("conv-title");

    let mut conv = Conversation::with_id(s.clone());
    conv.add_message(oneai_core::Message::user("please summarize X".to_string()));
    store.save_conversation(&s, &conv).await.unwrap();

    // Title derived from the first user message; promoted into metadata on
    // load (so a resave preserves it).
    let loaded = store.load_conversation(&s).await.unwrap().unwrap();
    assert_eq!(
        loaded.metadata.get("title").map(String::as_str),
        Some("please summarize X")
    );

    // Targeted rename (metadata-only UPDATE).
    store
        .rename_conversation(&s, "My Renamed Session")
        .await
        .unwrap();
    let loaded = store.load_conversation(&s).await.unwrap().unwrap();
    assert_eq!(
        loaded.metadata.get("title").map(String::as_str),
        Some("My Renamed Session")
    );
    // Empty rename is a no-op.
    store.rename_conversation(&s, "   ").await.unwrap();
    // Missing row is an error.
    assert!(store
        .rename_conversation(&format!("{s}-nope"), "x")
        .await
        .is_err());

    // The rename SURVIVES a resave of the live conversation whose in-memory
    // metadata lacks the title (save_conversation merges: DB metadata is the
    // base, incoming wins only for keys it carries).
    store.save_conversation(&s, &conv).await.unwrap();
    let loaded = store.load_conversation(&s).await.unwrap().unwrap();
    assert_eq!(
        loaded.metadata.get("title").map(String::as_str),
        Some("My Renamed Session")
    );

    // Archive toggle via metadata, and un-archive removes the key.
    store.set_conversation_archived(&s, true).await.unwrap();
    let loaded = store.load_conversation(&s).await.unwrap().unwrap();
    assert_eq!(
        loaded.metadata.get("archived").map(String::as_str),
        Some("1")
    );
    store.set_conversation_archived(&s, false).await.unwrap();
    let loaded = store.load_conversation(&s).await.unwrap().unwrap();
    assert!(!loaded.metadata.contains_key("archived"));

    cleanup(&store, &s).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres with pgvector)"]
async fn conversation_list_folds_and_flags_scoped() {
    let Some(store) = pg_store().await else {
        return;
    };
    let s = scope("conv-list");
    let (id1, id2) = (format!("{s}-a"), format!("{s}-b"));

    // id1: user + TWO consecutive assistant messages = 2 visible bubbles
    // (the folded display count — issue #17 parity with the SQLite backend).
    let mut conv1 = Conversation::with_id(id1.clone());
    conv1.add_message(oneai_core::Message::user("topic".to_string()));
    conv1.add_message(oneai_core::Message::assistant("thinking…".to_string()));
    conv1.add_message(oneai_core::Message::assistant("answer".to_string()));
    conv1
        .metadata
        .insert("workspace".to_string(), "/tmp/ws1".to_string());
    store.save_conversation(&id1, &conv1).await.unwrap();

    // id2: archived + workspace flag.
    let mut conv2 = Conversation::with_id(id2.clone());
    conv2.add_message(oneai_core::Message::user("hi".to_string()));
    conv2
        .metadata
        .insert("workspace".to_string(), "/tmp/ws2".to_string());
    store.save_conversation(&id2, &conv2).await.unwrap();
    store.set_conversation_archived(&id2, true).await.unwrap();

    let sessions = store.list_conversations().await.unwrap();
    let scoped: Vec<_> = sessions.iter().filter(|si| si.id.starts_with(&s)).collect();
    assert_eq!(scoped.len(), 2);
    let one = scoped.iter().find(|si| si.id == id1).unwrap();
    let two = scoped.iter().find(|si| si.id == id2).unwrap();
    assert_eq!(
        one.message_count, 2,
        "two assistant msgs fold into one bubble"
    );
    assert_eq!(one.workspace.as_deref(), Some("/tmp/ws1"));
    assert!(!one.archived);
    assert_eq!(two.message_count, 1);
    assert!(two.archived);

    // Delete removes the row (and its STM).
    store.delete_conversation(&id1).await.unwrap();
    assert!(store.load_conversation(&id1).await.unwrap().is_none());

    cleanup(&store, &s).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres with pgvector)"]
async fn discarded_snapshots_ordered_hidden_and_cascade_deleted() {
    let Some(store) = pg_store().await else {
        return;
    };
    let s = scope("conv-disc");

    let mut live = Conversation::with_id(s.clone());
    live.add_message(oneai_core::Message::user("latest question".to_string()));
    store.save_conversation(&s, &live).await.unwrap();

    // Two archive snapshots under the `{id}::discarded::{uuid}` convention.
    let snap1_id = format!("{s}{DISCARDED_SNAPSHOT_MARKER}00000000-0000-0000-0000-000000000001");
    let snap2_id = format!("{s}{DISCARDED_SNAPSHOT_MARKER}00000000-0000-0000-0000-000000000002");
    let mut snap1 = Conversation::with_id(snap1_id.clone());
    snap1.add_message(oneai_core::Message::system("sys prompt".to_string()));
    snap1.add_message(oneai_core::Message::user("old q1".to_string()));
    snap1.add_message(oneai_core::Message::assistant("old a1".to_string()));
    store.save_conversation(&snap1_id, &snap1).await.unwrap();
    // created_at ordering must be deterministic (TIMESTAMPTZ is µs-precise).
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let mut snap2 = Conversation::with_id(snap2_id.clone());
    snap2.add_message(oneai_core::Message::user("old q2".to_string()));
    store.save_conversation(&snap2_id, &snap2).await.unwrap();

    // Ordered oldest-first, messages intact.
    let snaps = store.load_discarded_snapshots(&s).await.unwrap();
    assert_eq!(snaps.len(), 2);
    assert_eq!(snaps[0].id, snap1_id);
    assert_eq!(snaps[1].id, snap2_id);
    assert_eq!(snaps[0].messages.len(), 3);

    // Display counts exclude `system`, oldest-first: [2, 1].
    let counts = store.snapshot_display_counts(&s).await.unwrap();
    assert_eq!(counts.len(), 2);
    assert_eq!(counts[0], (snap1_id.clone(), 2));
    assert_eq!(counts[1], (snap2_id.clone(), 1));

    // Snapshots are hidden from list_conversations (bucketed into the parent).
    let sessions = store.list_conversations().await.unwrap();
    let scoped: Vec<_> = sessions.iter().filter(|si| si.id.starts_with(&s)).collect();
    assert_eq!(scoped.len(), 1, "only the live row is listed");
    assert_eq!(scoped[0].id, s);

    // delete_conversation cascades to the snapshots + their STM.
    store.delete_conversation(&s).await.unwrap();
    assert!(store.load_discarded_snapshots(&s).await.unwrap().is_empty());
    assert!(store.load_conversation(&snap1_id).await.unwrap().is_none());

    cleanup(&store, &s).await;
}

// ─── Facts ───────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres with pgvector)"]
async fn facts_roundtrip_upsert_and_scoping() {
    let Some(store) = pg_store().await else {
        return;
    };
    let s = scope("facts");

    let mut fact = make_fact(&s, "user.package_manager", "prefers", "pnpm");
    fact.embedding = Some(vec![0.5, 0.5, 0.0]);
    fact.importance = 0.8;
    fact.pinned = true;
    fact.metadata
        .insert("source".to_string(), "turn-3".to_string());
    store.store_fact(&fact).await.unwrap();

    let loaded = store.load_facts(&s, "s1").await.unwrap();
    assert_eq!(loaded.len(), 1);
    let f = &loaded[0];
    assert_eq!(f.content, "pnpm");
    assert_eq!(f.fact_type.as_str(), "user_tooling_pref");
    assert_eq!(f.embedding.as_ref().unwrap(), &vec![0.5, 0.5, 0.0]);
    assert!((f.importance - 0.8).abs() < 1e-9);
    assert!(f.pinned);
    assert!(!f.superseded);
    assert_eq!(f.version, 1);
    assert_eq!(f.metadata.get("source").map(String::as_str), Some("turn-3"));

    // Conflict-resolved upsert: same (user, subject, predicate) → update +
    // version bump (Mem0 invariant), preserving the original id.
    let mut updated = make_fact(&s, "user.package_manager", "prefers", "bun");
    updated.id = "different-id".to_string();
    updated.superseded = true;
    updated.superseded_at = Some(chrono::Utc::now());
    store.store_fact(&updated).await.unwrap();

    let loaded = store.load_facts(&s, "s1").await.unwrap();
    assert_eq!(loaded.len(), 1, "upsert must not duplicate");
    let f = &loaded[0];
    assert_eq!(f.content, "bun");
    assert_eq!(f.version, 2);
    assert!(f.superseded);
    assert!(f.superseded_at.is_some());
    assert_eq!(f.id, fact.id, "original id preserved");

    // Scoping: a second session's fact is hidden from the first session's
    // scoped query but visible in the cross-session (empty session) query.
    let mut other = make_fact(&s, "user.editor", "prefers", "helix");
    other.session_id = "s2".to_string();
    store.store_fact(&other).await.unwrap();
    assert_eq!(store.load_facts(&s, "s1").await.unwrap().len(), 1);
    assert_eq!(store.load_facts(&s, "s2").await.unwrap().len(), 1);
    assert_eq!(store.load_facts(&s, "").await.unwrap().len(), 2);

    cleanup(&store, &s).await;
}

// ─── Multi-writer + restart properties (the point of the Pg backend) ────────

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres with pgvector)"]
async fn concurrent_writers_do_not_corrupt() {
    let Some(store) = pg_store().await else {
        return;
    };
    let s = scope("conc");

    // 10 concurrent tasks on separate pool clients: distinct LTM inserts +
    // per-session STM rewrites (transactional DELETE+INSERT).
    let mut handles = Vec::new();
    for i in 0..10 {
        let store = PgMemoryStore::new(store.pool().clone());
        let s = s.clone();
        handles.push(tokio::spawn(async move {
            store
                .save_ltm(&make_entry(
                    &format!("{s}-ltm-{i}"),
                    &format!("fact {i}"),
                    None,
                ))
                .await
                .unwrap();
            store
                .save_stm(
                    &format!("{s}-sess-{i}"),
                    &[make_entry(&format!("{s}-stm-{i}"), "x", None)],
                )
                .await
                .unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    for i in 0..10 {
        assert!(store
            .load_ltm(&format!("{s}-ltm-{i}"))
            .await
            .unwrap()
            .is_some());
        assert_eq!(
            store
                .load_stm(&format!("{s}-sess-{i}"))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    cleanup(&store, &s).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres with pgvector)"]
async fn survives_reconnect() {
    // The whole point of externalization: a fresh container (new pool, empty
    // volume) sees the previous one's memory.
    let Some(store) = pg_store().await else {
        return;
    };
    let s = scope("restart");
    let dsn = std::env::var("ONEAI_TEST_PG_DSN").unwrap();

    let mut conv = Conversation::with_id(s.clone());
    conv.add_message(oneai_core::Message::user("remember me".to_string()));
    store.save_conversation(&s, &conv).await.unwrap();
    store
        .save_ltm(&make_entry(&format!("{s}-ltm"), "durable", None))
        .await
        .unwrap();
    drop(store);

    let store2 = PgMemoryStore::connect(&dsn).await.unwrap();
    let loaded = store2.load_conversation(&s).await.unwrap().unwrap();
    assert_eq!(loaded.messages.len(), 1);
    assert!(store2
        .load_ltm(&format!("{s}-ltm"))
        .await
        .unwrap()
        .is_some());

    cleanup(&store2, &s).await;
}
