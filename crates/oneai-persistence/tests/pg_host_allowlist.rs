//! PgHostAllowlist integration tests (MVS3-B storage externalization).
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
//!   --test pg_host_allowlist -- --ignored
//! ```
//!
//! Coverage mirrors the `SqliteHostAllowlist` unit tests (mutual exclusion,
//! reopen persistence, list/remove CRUD). Each test uses hosts under a unique
//! scope suffix so a shared test DB stays unambiguous; list assertions filter
//! to the scope.

#![cfg(feature = "postgres")]

use oneai_core::HostAllowlistStore;
use oneai_persistence::PgHostAllowlist;

/// Connect when `ONEAI_TEST_PG_DSN` is set; `None` = skip the test.
async fn pg_store() -> Option<PgHostAllowlist> {
    let dsn = std::env::var("ONEAI_TEST_PG_DSN")
        .ok()
        .filter(|s| !s.is_empty())?;
    Some(
        PgHostAllowlist::connect(&dsn)
            .await
            .expect("ONEAI_TEST_PG_DSN is set but the connection failed — is Postgres running?"),
    )
}

/// A unique host-scope suffix per test run.
fn scope(label: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{label}-{nanos}")
}

/// Scoped host name (`{name}.{scope}.example` — always lower-case storage).
fn host(scope: &str, name: &str) -> String {
    format!("{name}.{scope}.example")
}

async fn cleanup(store: &PgHostAllowlist, scope: &str, names: &[&str]) {
    for n in names {
        store.remove(&host(scope, n)).await;
        store.remove_denied(&host(scope, n)).await;
    }
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn add_then_allowed_persists() {
    let Some(store) = pg_store().await else {
        return;
    };
    let s = scope("allow");
    let h = host(&s, "www");

    assert!(!store.is_allowed(&h).await);
    // Case normalized on write AND on read.
    store.add(h.to_uppercase()).await;
    assert!(store.is_allowed(&h).await);
    assert!(store.is_allowed(&h.to_uppercase()).await);

    cleanup(&store, &s, &["www"]).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn add_denied_then_denied_persists() {
    let Some(store) = pg_store().await else {
        return;
    };
    let s = scope("deny");
    let h = host(&s, "evil");

    assert!(!store.is_denied(&h).await);
    store.add_denied(h.clone()).await;
    assert!(store.is_denied(&h).await);

    cleanup(&store, &s, &["evil"]).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn admit_removes_prior_denial() {
    let Some(store) = pg_store().await else {
        return;
    };
    let s = scope("flip1");
    let h = host(&s, "flaky");

    store.add_denied(h.clone()).await;
    assert!(store.is_denied(&h).await);
    store.add(h.clone()).await;
    assert!(!store.is_denied(&h).await, "admit clears the stale denial");
    assert!(store.is_allowed(&h).await);

    cleanup(&store, &s, &["flaky"]).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn deny_removes_prior_admission() {
    let Some(store) = pg_store().await else {
        return;
    };
    let s = scope("flip2");
    let h = host(&s, "once-ok");

    store.add(h.clone()).await;
    assert!(store.is_allowed(&h).await);
    store.add_denied(h.clone()).await;
    assert!(
        !store.is_allowed(&h).await,
        "deny clears the stale admission"
    );
    assert!(store.is_denied(&h).await);

    cleanup(&store, &s, &["once-ok"]).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn distinct_hosts_do_not_cross_contaminate() {
    let Some(store) = pg_store().await else {
        return;
    };
    let s = scope("distinct");
    let (a, b) = (host(&s, "a"), host(&s, "b"));

    store.add(a.clone()).await;
    store.add_denied(b.clone()).await;
    assert!(store.is_allowed(&a).await);
    assert!(!store.is_denied(&a).await);
    assert!(store.is_denied(&b).await);
    assert!(!store.is_allowed(&b).await);

    cleanup(&store, &s, &["a", "b"]).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn survives_reopen() {
    // The whole point of the shared Pg store: a FRESH CONTAINER (new pool,
    // empty volume) sees what the previous one admitted/denied.
    let Some(store) = pg_store().await else {
        return;
    };
    let s = scope("reopen");
    let dsn = std::env::var("ONEAI_TEST_PG_DSN").unwrap();
    let (allowed, denied) = (host(&s, "persisted"), host(&s, "blocked"));

    store.add(allowed.clone()).await;
    store.add_denied(denied.clone()).await;
    drop(store);

    let store2 = PgHostAllowlist::connect(&dsn).await.unwrap();
    assert!(store2.is_allowed(&allowed).await);
    assert!(store2.is_denied(&denied).await);

    cleanup(&store2, &s, &["persisted", "blocked"]).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn list_ordered_and_populated() {
    let Some(store) = pg_store().await else {
        return;
    };
    let s = scope("list");

    store.add(host(&s, "beta")).await;
    store.add(host(&s, "alpha")).await; // ordering check
    store.add_denied(host(&s, "evil")).await;

    // Scoped view (the shared DB may hold sibling rows).
    let suffix = format!(".{s}.example");
    let allowed: Vec<_> = store
        .list_allowed()
        .await
        .into_iter()
        .filter(|e| e.host.ends_with(&suffix))
        .collect();
    let denied: Vec<_> = store
        .list_denied()
        .await
        .into_iter()
        .filter(|e| e.host.ends_with(&suffix))
        .collect();

    assert_eq!(allowed.len(), 2);
    // ORDER BY host ASC within the scope.
    assert!(allowed[0].host.contains("alpha"));
    assert!(allowed[1].host.contains("beta"));
    assert!(allowed[0].recorded_at_ms > 0);
    // recorded_at is unix seconds on the wire ×1000 → sane epoch-millis.
    assert!(allowed[0].recorded_at_ms > 1_700_000_000_000);
    assert_eq!(denied.len(), 1);
    assert!(denied[0].host.contains("evil"));

    cleanup(&store, &s, &["alpha", "beta", "evil"]).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn remove_clears_row_and_is_idempotent() {
    let Some(store) = pg_store().await else {
        return;
    };
    let s = scope("remove");
    let (ok, bad) = (host(&s, "once-ok"), host(&s, "bad"));

    store.add(ok.clone()).await;
    store.add_denied(bad.clone()).await;
    assert!(store.is_allowed(&ok).await);

    store.remove(&ok).await;
    assert!(!store.is_allowed(&ok).await);
    store.remove_denied(&bad).await;
    assert!(!store.is_denied(&bad).await);

    // Idempotent for missing hosts (no panic, no error).
    store.remove(&host(&s, "never-seen")).await;
    store.remove_denied(&host(&s, "never-seen")).await;

    cleanup(&store, &s, &["once-ok", "bad"]).await;
}
