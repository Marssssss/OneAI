//! PgFeedbackStore integration tests (MVS3-C storage externalization).
//!
//! Gated twice, mirroring `tests/pg_working_state.rs`:
//! 1. Compile-time: the whole file is empty unless `--features postgres`.
//! 2. Run-time: `#[ignore]` + `ONEAI_TEST_PG_DSN` — CI never needs Postgres.
//!
//! ```text
//! docker run -d --name oneai-pg-test -p 5432:5432 \
//!   -e POSTGRES_PASSWORD=oneai -e POSTGRES_DB=oneai_test pgvector/pgvector:pg16
//! ONEAI_TEST_PG_DSN=postgres://postgres:oneai@127.0.0.1:5432/oneai_test \
//!   cargo test -p oneai-persistence --features postgres \
//!   --test pg_feedback_store -- --ignored
//! ```
//!
//! Coverage mirrors the `SqliteSessionStore` feedback unit test
//! (`sqlite_store.rs` — record/list round-trip, session scoping, note text
//! preservation, oldest-first ordering), plus the reconnect-survival
//! property the externalization exists for.

#![cfg(feature = "postgres")]

use oneai_persistence::PgFeedbackStore;

/// Connect when `ONEAI_TEST_PG_DSN` is set; `None` = skip the test.
async fn pg_store() -> Option<PgFeedbackStore> {
    let dsn = std::env::var("ONEAI_TEST_PG_DSN")
        .ok()
        .filter(|s| !s.is_empty())?;
    Some(
        PgFeedbackStore::connect(&dsn)
            .await
            .expect("ONEAI_TEST_PG_DSN is set but the connection failed — is Postgres running?"),
    )
}

/// A unique session scope per test run so concurrent / repeated runs against
/// the same DB never see each other's rows.
fn scope(label: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("pgfb-{label}-{nanos}")
}

/// Distinct `created_at_ms` values make the ordering assertions
/// deterministic (epoch-millis granularity — a 2ms gap is plenty).
async fn tick() {
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
}

async fn cleanup(store: &PgFeedbackStore, sessions: &[&str]) {
    let c = store.pool().get().await.expect("pool");
    for s in sessions {
        c.execute(
            "DELETE FROM message_feedback_pg WHERE session_id = $1",
            &[s],
        )
        .await
        .expect("cleanup");
    }
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn record_list_roundtrip_and_scoping() {
    let Some(store) = pg_store().await else {
        return;
    };
    let s1 = scope("s1");
    let s2 = scope("s2");

    // Same shape as the SQLite test: up on t1, note on t2 (s1); down on t9 (s2).
    store.record(&s1, "t1", "assistant", "up", None).await;
    tick().await;
    store
        .record(&s1, "t2", "assistant", "note", Some("nice"))
        .await;
    tick().await;
    store.record(&s2, "t9", "assistant", "down", None).await;

    let l1 = store.list(&s1).await;
    assert_eq!(l1.len(), 2, "s1 must see exactly its two entries");
    // Oldest-first ordering (created_at_ms ASC).
    assert_eq!(l1[0].turn_id, "t1");
    assert_eq!(l1[0].kind, "up");
    assert_eq!(l1[0].text, None, "non-note kinds carry no text");
    assert_eq!(l1[0].message_role, "assistant");
    assert!(l1[0].id.starts_with("fb-"), "store-assigned id shape");
    assert_eq!(l1[1].turn_id, "t2");
    assert_eq!(l1[1].kind, "note");
    assert_eq!(l1[1].text.as_deref(), Some("nice"), "note text preserved");
    assert!(
        l1[0].created_at_ms <= l1[1].created_at_ms,
        "timestamps monotonic"
    );

    let l2 = store.list(&s2).await;
    assert_eq!(l2.len(), 1);
    assert_eq!(l2[0].turn_id, "t9");
    assert_eq!(l2[0].kind, "down");

    cleanup(&store, &[&s1, &s2]).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn absent_session_lists_empty() {
    let Some(store) = pg_store().await else {
        return;
    };
    assert!(store.list(&scope("never-written")).await.is_empty());
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn feedback_survives_reconnect() {
    let Some(store) = pg_store().await else {
        return;
    };
    let sess = scope("reconnect");
    store
        .record(&sess, "t1", "assistant", "note", Some("跨容器存活"))
        .await;

    // A second store instance (fresh pool) against the same DB sees the row
    // — the container/volume-loss recovery property.
    let dsn = std::env::var("ONEAI_TEST_PG_DSN").unwrap();
    let store2 = PgFeedbackStore::connect(&dsn).await.expect("reconnect");
    let l = store2.list(&sess).await;
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].text.as_deref(), Some("跨容器存活"));

    cleanup(&store, &[&sess]).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn concurrent_records_are_all_persisted() {
    let Some(store) = pg_store().await else {
        return;
    };
    let sess = scope("concurrent");
    let store = std::sync::Arc::new(store);

    let mut handles = Vec::new();
    for i in 0..8 {
        let s = store.clone();
        let id = sess.clone();
        handles.push(tokio::spawn(async move {
            s.record(&id, &format!("t{i}"), "assistant", "up", None)
                .await;
        }));
    }
    for h in handles {
        h.await.expect("task");
    }

    let l = store.list(&sess).await;
    assert_eq!(l.len(), 8, "no record may be lost");
    let mut turns: Vec<_> = l.iter().map(|e| e.turn_id.clone()).collect();
    turns.sort();
    let expected: Vec<String> = (0..8).map(|i| format!("t{i}")).collect();
    assert_eq!(turns, expected);

    cleanup(&store, &[&sess]).await;
}
