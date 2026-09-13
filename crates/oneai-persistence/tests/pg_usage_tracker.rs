//! PgUsageTracker integration tests (MVS3-B storage externalization).
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
//!   --test pg_usage_tracker -- --ignored
//! ```
//!
//! Coverage mirrors the `SqliteUsageTracker` unit tests. Session-scoped tests
//! use a unique session id per run; the global-aggregation tests assert on
//! BEFORE/AFTER DELTAS (a shared test DB carries sibling rows). `clear_all`
//! is global by contract — deliberately NOT exercised here.

#![cfg(feature = "postgres")]

use oneai_core::usage::{UsageRecord, UsageTracker};
use oneai_persistence::PgUsageTracker;

/// Connect when `ONEAI_TEST_PG_DSN` is set; `None` = skip the test.
async fn pg_tracker() -> Option<PgUsageTracker> {
    let dsn = std::env::var("ONEAI_TEST_PG_DSN")
        .ok()
        .filter(|s| !s.is_empty())?;
    Some(
        PgUsageTracker::connect(&dsn)
            .await
            .expect("ONEAI_TEST_PG_DSN is set but the connection failed — is Postgres running?"),
    )
}

/// A unique session scope per test run so concurrent / repeated runs against
/// the same DB never see each other's records.
fn scope(label: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("pgusage-{label}-{nanos}")
}

async fn cleanup(tracker: &PgUsageTracker, session: &str) {
    tracker.clear_session(session).await.expect("cleanup");
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn record_and_session_usage() {
    let Some(tracker) = pg_tracker().await else {
        return;
    };
    let sess = scope("sess");

    tracker
        .record_usage(UsageRecord::new(&sess, "gpt-4o", "openai", 100, 50))
        .await
        .unwrap();
    tracker
        .record_usage(UsageRecord::new(
            &sess,
            "claude-sonnet-4",
            "anthropic",
            200,
            100,
        ))
        .await
        .unwrap();

    let usage = tracker.session_usage(&sess).await.unwrap();
    assert_eq!(usage.call_count, 2);
    assert_eq!(usage.total_tokens, 450);

    cleanup(&tracker, &sess).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn global_reads_include_scoped_records() {
    let Some(tracker) = pg_tracker().await else {
        return;
    };
    let sess = scope("global");

    // Global-count DELTA assertions are racy on a shared test DB (sibling
    // tests insert AND scope-delete their own rows concurrently), so this
    // asserts the global read paths surface this scope's own records:
    // containment + monotonic lower bounds (both race-free).
    tracker
        .record_usage(UsageRecord::new(&sess, "gpt-4o", "openai", 100, 50))
        .await
        .unwrap();
    tracker
        .record_usage(UsageRecord::new(&sess, "gpt-4o", "openai", 100, 50))
        .await
        .unwrap();

    // global_records spans every session and carries the scoped rows.
    let global = tracker.global_records().await.unwrap();
    let mine: Vec<_> = global.iter().filter(|r| r.session_id == sess).collect();
    assert_eq!(mine.len(), 2);
    // Ordered by timestamp ASC overall.
    assert!(global.windows(2).all(|w| w[0].timestamp <= w[1].timestamp));

    // Global summary aggregates at least this scope's contribution.
    let summary = tracker.global_usage().await.unwrap();
    assert!(summary.call_count >= 2);
    assert!(summary.total_tokens >= 300);

    // Per-model global breakdown includes the model with ≥ my tokens.
    let by_model = tracker.usage_by_model_global().await.unwrap();
    assert!(by_model["gpt-4o"].total_tokens >= 300);

    cleanup(&tracker, &sess).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn per_model_breakdown() {
    let Some(tracker) = pg_tracker().await else {
        return;
    };
    let sess = scope("models");

    tracker
        .record_usage(UsageRecord::new(&sess, "gpt-4o", "openai", 100, 50))
        .await
        .unwrap();
    tracker
        .record_usage(UsageRecord::new(
            &sess,
            "claude-sonnet-4",
            "anthropic",
            200,
            100,
        ))
        .await
        .unwrap();

    let by_model = tracker.usage_by_model(&sess).await.unwrap();
    assert_eq!(by_model.len(), 2);
    assert!(by_model.contains_key("gpt-4o"));
    assert!(by_model.contains_key("claude-sonnet-4"));
    assert_eq!(by_model["gpt-4o"].total_tokens, 150);

    // Global breakdown contains at least this session's models.
    let global = tracker.usage_by_model_global().await.unwrap();
    assert!(global.contains_key("gpt-4o"));

    cleanup(&tracker, &sess).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn session_records_and_clear() {
    let Some(tracker) = pg_tracker().await else {
        return;
    };
    let (sess1, sess2) = (scope("rec1"), scope("rec2"));

    tracker
        .record_usage(UsageRecord::new(&sess1, "gpt-4o", "openai", 100, 50))
        .await
        .unwrap();
    tracker
        .record_usage(UsageRecord::new(
            &sess1,
            "claude-sonnet-4",
            "anthropic",
            200,
            100,
        ))
        .await
        .unwrap();
    tracker
        .record_usage(UsageRecord::new(&sess2, "gpt-4o", "openai", 10, 5))
        .await
        .unwrap();

    let records = tracker.session_records(&sess1).await.unwrap();
    assert_eq!(records.len(), 2);
    // Ordered by timestamp ASC.
    assert!(records[0].timestamp <= records[1].timestamp);

    // clear_session only touches its own scope.
    tracker.clear_session(&sess1).await.unwrap();
    assert_eq!(tracker.session_usage(&sess1).await.unwrap().call_count, 0);
    assert_eq!(tracker.session_records(&sess2).await.unwrap().len(), 1);

    cleanup(&tracker, &sess2).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn cache_tokens_roundtrip() {
    let Some(tracker) = pg_tracker().await else {
        return;
    };
    let sess = scope("cache");

    tracker
        .record_usage(
            UsageRecord::new(&sess, "claude-sonnet-4", "anthropic", 1000, 200)
                .with_cache_tokens(800, 50),
        )
        .await
        .unwrap();

    let records = tracker.session_records(&sess).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].cache_read_tokens, 800);
    assert_eq!(records[0].cache_creation_tokens, 50);

    let summary = tracker.session_usage(&sess).await.unwrap();
    assert_eq!(summary.cache_read_tokens, 800);
    assert_eq!(summary.cache_creation_tokens, 50);
    assert!(summary.cache_hit_ratio() > 0.0);

    cleanup(&tracker, &sess).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn is_estimated_roundtrip() {
    // The SQLite table never grew this column (the flag is lost across a
    // SQLite round-trip); Pg persists it faithfully.
    let Some(tracker) = pg_tracker().await else {
        return;
    };
    let sess = scope("est");

    let mut estimated = UsageRecord::new(&sess, "glm-4.6", "zhipu", 100, 50);
    estimated.is_estimated = true;
    tracker.record_usage(estimated).await.unwrap();
    tracker
        .record_usage(UsageRecord::new(&sess, "gpt-4o", "openai", 10, 5))
        .await
        .unwrap();

    let records = tracker.session_records(&sess).await.unwrap();
    assert_eq!(records.len(), 2);
    let est = records.iter().find(|r| r.model == "glm-4.6").unwrap();
    let real = records.iter().find(|r| r.model == "gpt-4o").unwrap();
    assert!(est.is_estimated);
    assert!(!real.is_estimated);

    cleanup(&tracker, &sess).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn timestamp_and_metadata_roundtrip() {
    let Some(tracker) = pg_tracker().await else {
        return;
    };
    let sess = scope("meta");

    // µs-aligned fixed timestamp (TIMESTAMPTZ stores microseconds).
    let ts = chrono::DateTime::from_timestamp(1_800_000_000, 123_456_000).unwrap();
    let mut record = UsageRecord::with_timestamp(
        &sess,
        "gpt-4o",
        "openai",
        42,
        7,
        ts,
        std::collections::HashMap::from([("kind".to_string(), "tool-call".to_string())]),
    );
    record.metadata.insert("extra".to_string(), "x".to_string());
    tracker.record_usage(record).await.unwrap();

    let records = tracker.session_records(&sess).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].timestamp, ts);
    assert_eq!(records[0].prompt_tokens, 42);
    assert_eq!(
        records[0].metadata.get("kind").map(String::as_str),
        Some("tool-call")
    );
    assert_eq!(
        records[0].metadata.get("extra").map(String::as_str),
        Some("x")
    );

    cleanup(&tracker, &sess).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn survives_reconnect() {
    // The point of externalization: a fresh container sees the ledger.
    let Some(tracker) = pg_tracker().await else {
        return;
    };
    let sess = scope("restart");
    let dsn = std::env::var("ONEAI_TEST_PG_DSN").unwrap();

    tracker
        .record_usage(UsageRecord::new(&sess, "gpt-4o", "openai", 100, 50))
        .await
        .unwrap();
    drop(tracker);

    let tracker2 = PgUsageTracker::connect(&dsn).await.unwrap();
    let usage = tracker2.session_usage(&sess).await.unwrap();
    assert_eq!(usage.call_count, 1);
    assert_eq!(usage.total_tokens, 150);

    cleanup(&tracker2, &sess).await;
}
