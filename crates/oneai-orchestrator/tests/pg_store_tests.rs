//! PgSessionStore integration tests (MVS4-A multi-replica routing table).
//!
//! Gated twice, mirroring `oneai-persistence/tests/pg_session_event_store.rs`:
//! 1. Compile-time: the whole file is empty unless `--features postgres`.
//! 2. Run-time: `#[ignore]` + `ONEAI_TEST_PG_DSN` — CI never needs Postgres.
//!
//! ```text
//! docker run -d --name oneai-pg-test -p 5432:5432 \
//!   -e POSTGRES_PASSWORD=oneai -e POSTGRES_DB=oneai_test pgvector/pgvector:pg16
//! ONEAI_TEST_PG_DSN=postgres://postgres:oneai@127.0.0.1:5432/oneai_test \
//!   cargo test -p oneai-orchestrator --features postgres \
//!   --test pg_store_tests -- --ignored
//! ```
//!
//! Coverage mirrors the FileSessionStore unit tests plus the multi-replica
//! primitives a file can't give: cross-connection CAS arbitration, archive
//! claim/release exclusivity, lease claim/renew/expire-takeover against real
//! row locks + server-side `now()`, and monotonic activity flushes.

#![cfg(feature = "postgres")]

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use oneai_orchestrator::fsm::{PersistedEntry, SessionEntry, SessionState};
use oneai_orchestrator::runner::{ContainerHandle, SessionSpec};
use oneai_orchestrator::store::{ClaimOutcome, SessionStore};
use oneai_orchestrator::{ArchiveManifest, OrchestratorError, PgSessionStore};

/// Connect when `ONEAI_TEST_PG_DSN` is set; `None` = skip the test.
async fn pg_store() -> Option<PgSessionStore> {
    let dsn = std::env::var("ONEAI_TEST_PG_DSN")
        .ok()
        .filter(|s| !s.is_empty())?;
    Some(
        PgSessionStore::connect(&dsn)
            .await
            .expect("ONEAI_TEST_PG_DSN is set but the connection failed — is Postgres running?"),
    )
}

/// A unique session id per test run so concurrent / repeated runs against
/// the same DB never see each other's rows (session_id is the global PK).
fn scope(label: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("pgorch-{label}-{nanos}")
}

fn test_spec(id: &str) -> SessionSpec {
    SessionSpec {
        session_id: id.into(),
        image: "img".into(),
        state_volume: format!("oneai-orch-{id}-state"),
        workspace_volume: format!("oneai-orch-{id}-ws"),
        env: vec![("K".into(), "V".into())],
        bind_host: "127.0.0.1".into(),
        container_port: 8787,
        provider_config: None,
        created_at: Utc::now(),
    }
}

fn handle(port: u16) -> ContainerHandle {
    ContainerHandle {
        container_id: "cid".into(),
        container_name: "oneai-orch-pg".into(),
        host_port: port,
    }
}

fn persisted(id: &str) -> PersistedEntry {
    SessionEntry::new_creating(test_spec(id)).to_persisted()
}

fn manifest(id: &str) -> ArchiveManifest {
    ArchiveManifest {
        session_id: id.into(),
        archived_at: Utc::now().to_rfc3339(),
        volumes: vec![],
    }
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn connect_schema_idempotent() {
    let Some(dsn) = std::env::var("ONEAI_TEST_PG_DSN")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    // Two independent connects against the same DB: the second must be a
    // steady-state no-op (catalog probe), not a DDL deadlock.
    let s1 = PgSessionStore::connect(&dsn).await.unwrap();
    let s2 = PgSessionStore::connect(&dsn).await.unwrap();
    assert!(s1.load_all().await.is_ok());
    assert!(s2.load_all().await.is_ok());
    assert!(s2.supports_leasing());
    assert!(s2.store_path().is_none());
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn insert_get_roundtrip_and_duplicate() {
    let Some(store) = pg_store().await else {
        return;
    };
    let id = scope("ins");
    store.insert(&persisted(&id)).await.unwrap();

    let got = store.get(&id).await.unwrap().unwrap();
    assert_eq!(got.entry.state, SessionState::Creating);
    assert_eq!(got.entry.spec.env, vec![("K".into(), "V".into())]);
    assert!(got.entry.handle.is_none() && got.entry.archived.is_none());
    assert!(got.lease.is_none());
    assert_eq!(got.last_activity_ms, 0);

    // Duplicate id → AlreadyExists (the PK arbitrates across replicas).
    let dup = store.insert(&persisted(&id)).await;
    assert!(matches!(dup, Err(OrchestratorError::AlreadyExists(_))));

    store.remove(&id).await.unwrap();
    assert!(store.get(&id).await.unwrap().is_none());
    store.remove(&id).await.unwrap(); // idempotent
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn cas_transition_contract() {
    let Some(store) = pg_store().await else {
        return;
    };
    let id = scope("cas");
    store.insert(&persisted(&id)).await.unwrap();

    // Unknown id → None (even for an illegal pair).
    assert!(store
        .cas_transition(
            "pgorch-nope",
            SessionState::Running,
            SessionState::Creating,
            None,
            None
        )
        .await
        .unwrap()
        .is_none());
    // State mismatch → None.
    assert!(store
        .cas_transition(
            &id,
            SessionState::Running,
            SessionState::Hibernating,
            None,
            None
        )
        .await
        .unwrap()
        .is_none());
    // Expected matches but transition illegal → Err.
    assert!(matches!(
        store
            .cas_transition(
                &id,
                SessionState::Creating,
                SessionState::Hibernating,
                None,
                None
            )
            .await,
        Err(OrchestratorError::IllegalTransition { .. })
    ));
    // Legal hit: merges handle, bumps updated_at (server clock).
    let out = store
        .cas_transition(
            &id,
            SessionState::Creating,
            SessionState::Running,
            Some(handle(45001)),
            None,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(out.state, SessionState::Running);
    assert_eq!(out.handle.as_ref().unwrap().host_port, 45001);
    // Handle preserved when transitioning without a new one.
    let out = store
        .cas_transition(
            &id,
            SessionState::Running,
            SessionState::Hibernating,
            None,
            Some("idle".into()),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(out.handle.as_ref().unwrap().host_port, 45001);
    assert_eq!(out.last_error.as_deref(), Some("idle"));

    store.remove(&id).await.unwrap();
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn cas_transition_concurrent_exactly_one_wins() {
    let Some(store) = pg_store().await else {
        return;
    };
    let store = Arc::new(store);
    let id = scope("casc");
    store.insert(&persisted(&id)).await.unwrap();

    let mut joins = Vec::new();
    for _ in 0..10 {
        let s = store.clone();
        let id = id.clone();
        joins.push(tokio::spawn(async move {
            s.cas_transition(
                &id,
                SessionState::Creating,
                SessionState::Running,
                Some(handle(1)),
                None,
            )
            .await
            .unwrap()
            .is_some()
        }));
    }
    let wins = futures::future::join_all(joins)
        .await
        .into_iter()
        .filter(|r| *r.as_ref().unwrap())
        .count();
    assert_eq!(wins, 1);
    store.remove(&id).await.unwrap();
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn cas_set_archived_claim_release_exclusive() {
    let Some(store) = pg_store().await else {
        return;
    };
    let id = scope("arch");
    store.insert(&persisted(&id)).await.unwrap();
    store
        .cas_transition(
            &id,
            SessionState::Creating,
            SessionState::Running,
            Some(handle(1)),
            None,
        )
        .await
        .unwrap()
        .unwrap();
    store
        .cas_transition(
            &id,
            SessionState::Running,
            SessionState::Hibernating,
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();

    // Claim from Hibernating: hit; clears the handle.
    let out = store
        .cas_set_archived(&id, Some(manifest(&id)))
        .await
        .unwrap()
        .unwrap();
    assert!(out.archived.is_some() && out.handle.is_none());
    assert_eq!(out.state, SessionState::Hibernating);
    // Second claim (even a byte-identical manifest): miss.
    assert!(store
        .cas_set_archived(&id, Some(manifest(&id)))
        .await
        .unwrap()
        .is_none());
    // Release: hit; second release: miss.
    assert!(store.cas_set_archived(&id, None).await.unwrap().is_some());
    assert!(store.cas_set_archived(&id, None).await.unwrap().is_none());
    store.remove(&id).await.unwrap();
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn lease_claim_renew_takeover_lifecycle() {
    // Two independent pools = two "replicas" (separate connections, shared
    // truth — exactly the production topology).
    let Some(dsn) = std::env::var("ONEAI_TEST_PG_DSN")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let a = PgSessionStore::connect(&dsn).await.unwrap();
    let b = PgSessionStore::connect(&dsn).await.unwrap();
    let id = scope("lease");
    a.insert(&persisted(&id)).await.unwrap();
    let ttl = Duration::from_secs(2);

    // r1 claims.
    assert!(matches!(
        a.try_claim_lease(&id, "r1", ttl).await.unwrap(),
        ClaimOutcome::Owned { .. }
    ));
    // r2 is refused while the lease is fresh, and learns the owner.
    match b.try_claim_lease(&id, "r2", ttl).await.unwrap() {
        ClaimOutcome::HeldByOther { owner_replica, .. } => assert_eq!(owner_replica, "r1"),
        other => panic!("expected HeldByOther, got {other:?}"),
    }
    // r1 renewal extends (and r1's own re-claim is a renewal, not a conflict).
    let exp1 = match a.try_claim_lease(&id, "r1", ttl).await.unwrap() {
        ClaimOutcome::Owned { lease_expires_at } => lease_expires_at,
        other => panic!("expected Owned, got {other:?}"),
    };
    assert!(a.renew_lease(&id, "r1", ttl).await.unwrap().unwrap() >= exp1);
    // r2 cannot renew r1's lease.
    assert!(b.renew_lease(&id, "r2", ttl).await.unwrap().is_none());
    // r2 cannot release r1's lease.
    b.release_lease(&id, "r2").await.unwrap();
    assert!(matches!(
        b.try_claim_lease(&id, "r2", ttl).await.unwrap(),
        ClaimOutcome::HeldByOther { .. }
    ));
    // Ownership scoping sees exactly our session.
    assert!(a.list_owned_ids("r1").await.unwrap().contains(&id));
    assert!(!a.list_owned_ids("r2").await.unwrap().contains(&id));

    // r1 dies (no release, no renewal): after TTL, r2 takes over via the
    // expired-lease CAS — server-side now(), no client clocks involved.
    tokio::time::sleep(Duration::from_millis(2300)).await;
    assert!(matches!(
        b.try_claim_lease(&id, "r2", ttl).await.unwrap(),
        ClaimOutcome::Owned { .. }
    ));
    // ...and r1's late renewal now fails (ownership moved).
    assert!(a.renew_lease(&id, "r1", ttl).await.unwrap().is_none());

    // Explicit release frees the session immediately.
    b.release_lease(&id, "r2").await.unwrap();
    assert!(matches!(
        a.try_claim_lease(&id, "r1", ttl).await.unwrap(),
        ClaimOutcome::Owned { .. }
    ));
    // Gone session → NotFound.
    a.remove(&id).await.unwrap();
    assert!(matches!(
        a.try_claim_lease(&id, "r1", ttl).await.unwrap(),
        ClaimOutcome::NotFound
    ));
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn lease_claim_concurrent_exactly_one_winner() {
    let Some(store) = pg_store().await else {
        return;
    };
    let store = Arc::new(store);
    let id = scope("leasec");
    store.insert(&persisted(&id)).await.unwrap();
    let mut joins = Vec::new();
    for i in 0..10 {
        let s = store.clone();
        let id = id.clone();
        joins.push(tokio::spawn(async move {
            matches!(
                s.try_claim_lease(&id, &format!("r{i}"), Duration::from_secs(30))
                    .await
                    .unwrap(),
                ClaimOutcome::Owned { .. }
            )
        }));
    }
    let wins = futures::future::join_all(joins)
        .await
        .into_iter()
        .filter(|r| *r.as_ref().unwrap())
        .count();
    assert_eq!(wins, 1);
    store.remove(&id).await.unwrap();
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn touch_activity_is_monotonic() {
    let Some(store) = pg_store().await else {
        return;
    };
    let id = scope("act");
    store.insert(&persisted(&id)).await.unwrap();
    store.touch_activity(&id, 5000).await.unwrap();
    assert_eq!(
        store.get(&id).await.unwrap().unwrap().last_activity_ms,
        5000
    );
    // An older flush from a lagging replica never regresses the clock.
    store.touch_activity(&id, 4000).await.unwrap();
    assert_eq!(
        store.get(&id).await.unwrap().unwrap().last_activity_ms,
        5000
    );
    store.touch_activity(&id, 6000).await.unwrap();
    assert_eq!(
        store.get(&id).await.unwrap().unwrap().last_activity_ms,
        6000
    );
    // Unknown id: silent no-op.
    store.touch_activity("pgorch-nope", 1).await.unwrap();
    store.remove(&id).await.unwrap();
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn force_update_set_last_error_and_cross_store_visibility() {
    let Some(dsn) = std::env::var("ONEAI_TEST_PG_DSN")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let a = PgSessionStore::connect(&dsn).await.unwrap();
    let b = PgSessionStore::connect(&dsn).await.unwrap();
    let id = scope("force");
    a.insert(&persisted(&id)).await.unwrap();

    let mut e = a.get(&id).await.unwrap().unwrap().entry;
    e.state = SessionState::Running;
    e.handle = Some(handle(45555));
    a.force_update(&e).await.unwrap();
    // Store B (another "replica") sees the blind write immediately.
    let got = b.get(&id).await.unwrap().unwrap().entry;
    assert_eq!(got.state, SessionState::Running);
    assert_eq!(got.handle.unwrap().host_port, 45555);

    b.set_last_error(&id, "boom").await.unwrap();
    assert_eq!(
        a.get(&id)
            .await
            .unwrap()
            .unwrap()
            .entry
            .last_error
            .as_deref(),
        Some("boom")
    );

    // load_all includes the row; flush() is a no-op barrier.
    assert!(a
        .load_all()
        .await
        .unwrap()
        .iter()
        .any(|s| s.entry.spec.session_id == id));
    a.flush().await.unwrap();
    b.remove(&id).await.unwrap();
}
