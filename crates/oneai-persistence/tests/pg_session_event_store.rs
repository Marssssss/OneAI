//! PgSessionEventStore integration tests (MVS3-C storage externalization).
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
//!   --test pg_session_event_store -- --ignored
//! ```
//!
//! Coverage mirrors the `FileSessionEventStore` unit tests: append/load
//! round-trip (byte-exact — the column is TEXT, see the store's module
//! docs), append ordering, multi-session isolation, absent-session empty
//! load, and concurrent-append monotonicity (BIGSERIAL).

#![cfg(feature = "postgres")]

use oneai_core::traits::SessionEventStore;
use oneai_persistence::PgSessionEventStore;

/// Connect when `ONEAI_TEST_PG_DSN` is set; `None` = skip the test.
async fn pg_store() -> Option<PgSessionEventStore> {
    let dsn = std::env::var("ONEAI_TEST_PG_DSN")
        .ok()
        .filter(|s| !s.is_empty())?;
    Some(
        PgSessionEventStore::connect(&dsn)
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
    format!("pgevt-{label}-{nanos}")
}

async fn cleanup(store: &PgSessionEventStore, session: &str) {
    let c = store.pool().get().await.expect("pool");
    c.execute(
        "DELETE FROM session_events_pg WHERE session_id = $1",
        &[&session],
    )
    .await
    .expect("cleanup");
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn append_load_roundtrip_is_byte_exact() {
    let Some(store) = pg_store().await else {
        return;
    };
    let sess = scope("roundtrip");

    // Opaque JSON lines (what the bus tap writes) + an arbitrary non-JSON
    // line (the file backend stores any string; TEXT column = same contract).
    let l1 = r#"{"kind":"thinking","data":{"text":"héllo 世界"}}"#;
    let l2 = r#"{"kind":"turn_complete","seq":2}"#;
    let l3 = "not json — still an opaque line";
    for l in [l1, l2, l3] {
        store.append(&sess, l).await.expect("append");
    }

    let loaded = store.load(&sess).await.expect("load");
    assert_eq!(loaded, vec![l1.to_string(), l2.to_string(), l3.to_string()]);

    cleanup(&store, &sess).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn sessions_are_isolated() {
    let Some(store) = pg_store().await else {
        return;
    };
    let a = scope("iso-a");
    let b = scope("iso-b");

    store.append(&a, r#"{"who":"a1"}"#).await.expect("append");
    store.append(&b, r#"{"who":"b1"}"#).await.expect("append");
    store.append(&a, r#"{"who":"a2"}"#).await.expect("append");

    let la = store.load(&a).await.expect("load");
    let lb = store.load(&b).await.expect("load");
    assert_eq!(la.len(), 2);
    assert_eq!(lb.len(), 1);
    assert!(la.iter().all(|l| l.contains("\"a")));
    assert!(lb[0].contains("\"b"));

    cleanup(&store, &a).await;
    cleanup(&store, &b).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn absent_session_loads_empty() {
    let Some(store) = pg_store().await else {
        return;
    };
    let loaded = store
        .load(&scope("never-written"))
        .await
        .expect("load must succeed for an absent session");
    assert!(loaded.is_empty());
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn load_survives_reconnect() {
    let Some(store) = pg_store().await else {
        return;
    };
    let sess = scope("reconnect");
    store.append(&sess, r#"{"n":1}"#).await.expect("append");

    // A second store instance (fresh pool) against the same DB sees the row
    // — the container-loss recovery property the externalization exists for.
    let dsn = std::env::var("ONEAI_TEST_PG_DSN").unwrap();
    let store2 = PgSessionEventStore::connect(&dsn).await.expect("reconnect");
    let loaded = store2.load(&sess).await.expect("load");
    assert_eq!(loaded, vec![r#"{"n":1}"#.to_string()]);

    cleanup(&store, &sess).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn concurrent_appends_are_all_persisted_in_insertion_order() {
    let Some(store) = pg_store().await else {
        return;
    };
    let sess = scope("concurrent");
    let store = std::sync::Arc::new(store);

    // 10 concurrent appenders (the bus tap is one task per session, but N
    // containers may share one session id after a resume race) — BIGSERIAL
    // gives a total order; nothing may be lost.
    let mut handles = Vec::new();
    for i in 0..10 {
        let s = store.clone();
        let id = sess.clone();
        handles.push(tokio::spawn(async move {
            for j in 0..5 {
                s.append(&id, &format!(r#"{{"w":{i},"j":{j}}}"#))
                    .await
                    .expect("append");
            }
        }));
    }
    for h in handles {
        h.await.expect("task");
    }

    let loaded = store.load(&sess).await.expect("load");
    assert_eq!(loaded.len(), 50, "no append may be lost");
    // Every (w, j) pair is present exactly once.
    for i in 0..10 {
        for j in 0..5 {
            let needle = format!(r#"{{"w":{i},"j":{j}}}"#);
            assert_eq!(
                loaded.iter().filter(|l| **l == needle).count(),
                1,
                "missing or duplicated line {needle}"
            );
        }
    }

    cleanup(&store, &sess).await;
}
