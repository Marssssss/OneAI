//! PgWorkingStateStore integration tests (MVS3 storage externalization).
//!
//! Gated twice, mirroring the repo's `#[ignore]` e2e precedent
//! (`oneai-orchestrator/tests/e2e_docker.rs`):
//! 1. Compile-time: the whole file is empty unless `--features postgres`.
//! 2. Run-time: `#[ignore]` + `ONEAI_TEST_PG_DSN` — CI never needs Postgres.
//!
//! Run against a disposable local Postgres:
//! ```text
//! docker run -d --name oneai-pg-test -p 5432:5432 \
//!   -e POSTGRES_PASSWORD=oneai -e POSTGRES_DB=oneai_test postgres:16
//! ONEAI_TEST_PG_DSN=postgres://postgres:oneai@127.0.0.1:5432/oneai_test \
//!   cargo test -p oneai-persistence --features postgres \
//!   --test pg_working_state -- --ignored
//! ```
//!
//! Coverage mirrors the 8 `FileWorkingStateStore` unit tests (the
//! partial-final-line crash test doesn't apply — transactions never leave a
//! half-written row) and replaces it with the multi-writer concurrency tests
//! that motivate the Pg backend (transactional brief index; per-task
//! `FOR UPDATE` serialization). Each test runs under a unique project scope
//! and cleans up its own rows, so a shared test DB stays tidy.

#![cfg(feature = "postgres")]

use std::sync::Arc;

use oneai_core::traits::WorkingStateStore;
use oneai_core::{
    Blocker, BlockerStatus, Decision, Note, Step, StepStatus, TaskEventPayload, TaskEventType,
    TaskStatus,
};
use oneai_persistence::PgWorkingStateStore;

/// Connect when `ONEAI_TEST_PG_DSN` is set; `None` = skip the test.
async fn pg_store() -> Option<PgWorkingStateStore> {
    let dsn = std::env::var("ONEAI_TEST_PG_DSN")
        .ok()
        .filter(|s| !s.is_empty())?;
    Some(
        PgWorkingStateStore::connect(&dsn)
            .await
            .expect("ONEAI_TEST_PG_DSN is set but the connection failed — is Postgres running?"),
    )
}

/// A unique project scope per test run (timestamp + label) so concurrent /
/// repeated runs against the same DB never see each other's tasks.
fn scope(label: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("pgtest-{label}-{nanos}")
}

/// Delete every row this test created (briefs by project scope, events by
/// their task ids) — runs at the END of each test; stale rows from a crashed
/// run are harmless because scopes are unique.
async fn cleanup(store: &PgWorkingStateStore, project: &str) {
    let client = store.pool().get().await.expect("pool client");
    client
        .execute(
            "DELETE FROM working_state_events WHERE task_id IN
               (SELECT task_id FROM working_state_briefs WHERE project = $1)",
            &[&project],
        )
        .await
        .expect("cleanup events");
    client
        .execute(
            "DELETE FROM working_state_briefs WHERE project = $1",
            &[&project],
        )
        .await
        .expect("cleanup briefs");
}

// ─── Mirrors of the file-backend unit tests ─────────────────────────────────

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn create_and_get_task() {
    let Some(store) = pg_store().await else {
        eprintln!("skipping: ONEAI_TEST_PG_DSN not set");
        return;
    };
    let project = scope("create-get");
    let id = store
        .create_task(
            "alice",
            &project,
            "refactor auth",
            "split into services",
            "sess1",
        )
        .await
        .unwrap();
    let state = store.get_task(&id).await.unwrap().unwrap();
    assert_eq!(state.goal, "refactor auth");
    assert_eq!(state.intent, "split into services");
    assert_eq!(state.status, TaskStatus::Active);
    assert!(state.steps.is_empty());
    // user/project are backfilled from the brief (events don't carry them).
    assert_eq!(state.user_id, "alice");
    assert_eq!(state.project, project);
    cleanup(&store, &project).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn step_lifecycle_projects() {
    let Some(store) = pg_store().await else {
        return;
    };
    let project = scope("steps");
    let id = store
        .create_task("u", &project, "g", "", "sess")
        .await
        .unwrap();
    store
        .append_event(
            &id,
            "sess",
            None,
            TaskEventType::StepAdded,
            TaskEventPayload::StepAdded {
                step: Step {
                    id: "s1".into(),
                    description: "write tests".into(),
                    status: StepStatus::Pending,
                    depends_on: vec![],
                    order: 1,
                    active_form: None,
                    updated_at: String::new(),
                },
            },
        )
        .await
        .unwrap();
    store
        .append_event(
            &id,
            "sess",
            None,
            TaskEventType::StepStatusChanged,
            TaskEventPayload::StepStatusChanged {
                step_id: "s1".into(),
                status: StepStatus::InProgress,
                active_form: Some("writing tests".into()),
            },
        )
        .await
        .unwrap();
    let state = store.get_task(&id).await.unwrap().unwrap();
    assert_eq!(state.steps.len(), 1);
    assert_eq!(state.steps[0].status, StepStatus::InProgress);
    assert_eq!(state.steps[0].active_form.as_deref(), Some("writing tests"));
    // The brief must carry the derived open-step count (1 step, not completed).
    let open = store.list_open_tasks("u", &project).await.unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].open_step_count, 1);
    cleanup(&store, &project).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn decisions_and_blockers_project() {
    let Some(store) = pg_store().await else {
        return;
    };
    let project = scope("dec-block");
    let id = store
        .create_task("u", &project, "g", "", "sess")
        .await
        .unwrap();
    store
        .append_event(
            &id,
            "sess",
            None,
            TaskEventType::DecisionMade,
            TaskEventPayload::DecisionMade {
                decision: Decision {
                    id: "d1".into(),
                    question: "Pg driver?".into(),
                    chosen: "deadpool-postgres".into(),
                    rationale: "pooled, lightweight".into(),
                    alternatives: vec!["sqlx".into()],
                    step_id: None,
                    ts: String::new(),
                },
            },
        )
        .await
        .unwrap();
    store
        .append_event(
            &id,
            "sess",
            None,
            TaskEventType::BlockerRaised,
            TaskEventPayload::BlockerRaised {
                blocker: Blocker {
                    id: "b1".into(),
                    description: "CI flaky".into(),
                    status: BlockerStatus::Open,
                    resolution: None,
                    step_id: None,
                    ts: String::new(),
                },
            },
        )
        .await
        .unwrap();
    store
        .append_event(
            &id,
            "sess",
            None,
            TaskEventType::BlockerResolved,
            TaskEventPayload::BlockerResolved {
                blocker_id: "b1".into(),
                resolution: "retried".into(),
            },
        )
        .await
        .unwrap();
    let state = store.get_task(&id).await.unwrap().unwrap();
    assert_eq!(state.decisions.len(), 1);
    assert_eq!(state.decisions[0].chosen, "deadpool-postgres");
    assert_eq!(state.blockers.len(), 1);
    assert_eq!(state.blockers[0].status, BlockerStatus::Resolved);
    cleanup(&store, &project).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn list_open_tasks_cross_session() {
    let Some(store) = pg_store().await else {
        return;
    };
    let project = scope("list-open");
    let a = store
        .create_task("alice", &project, "task A", "", "s1")
        .await
        .unwrap();
    let _b = store
        .create_task("bob", &project, "task B", "", "s2")
        .await
        .unwrap();
    let c = store
        .create_task("alice", &project, "task C", "", "s3")
        .await
        .unwrap();
    // Complete C — should drop out of the open list.
    store
        .append_event(
            &c,
            "s3",
            None,
            TaskEventType::TaskCompleted,
            TaskEventPayload::TaskStatus {},
        )
        .await
        .unwrap();
    let open = store.list_open_tasks("alice", &project).await.unwrap();
    assert_eq!(open.len(), 1, "bob's task filtered by user, C by status");
    assert_eq!(open[0].goal, "task A");
    assert_eq!(open[0].task_id, a);
    // Empty user/project filters are unconstrained (file-backend semantics):
    // scoping by project alone still sees both open tasks in this scope.
    let open_proj = store.list_open_tasks("", &project).await.unwrap();
    assert_eq!(open_proj.len(), 2);
    cleanup(&store, &project).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn compaction_preserves_derived_state() {
    let Some(store) = pg_store().await else {
        return;
    };
    let store = store.with_compaction(5, 2);
    let project = scope("compact");
    let id = store
        .create_task("u", &project, "g", "", "sess")
        .await
        .unwrap();
    for i in 0..6 {
        store
            .append_event(
                &id,
                "sess",
                None,
                TaskEventType::NoteAdded,
                TaskEventPayload::NoteAdded {
                    note: Note {
                        id: format!("n{i}"),
                        content: format!("note {i}"),
                        ts: String::new(),
                    },
                },
            )
            .await
            .unwrap();
    }
    store.compact_if_needed(&id).await.unwrap();
    // After compaction: 1 snapshot + 2 tail = 3 events; the derived state
    // must still have all 6 notes (folded into the snapshot).
    let state = store.get_task(&id).await.unwrap().unwrap();
    assert_eq!(state.notes.len(), 6);
    let events = store.read_events(&id).await.unwrap();
    assert!(events.len() <= 3, "log compacted to {}", events.len());
    assert_eq!(events[0].event_type, TaskEventType::Snapshot);
    // Idempotent: a second pass is a no-op (under threshold now).
    store.compact_if_needed(&id).await.unwrap();
    let state2 = store.get_task(&id).await.unwrap().unwrap();
    assert_eq!(state2.notes.len(), 6);
    cleanup(&store, &project).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn archive_marks_brief_and_keeps_events() {
    let Some(store) = pg_store().await else {
        return;
    };
    let project = scope("archive");
    let id = store
        .create_task("u", &project, "g", "", "sess")
        .await
        .unwrap();
    store.archive_task(&id).await.unwrap();
    // Dropped from the open list; brief status archived.
    let open = store.list_open_tasks("u", &project).await.unwrap();
    assert!(open.is_empty());
    let state = store.get_task(&id).await.unwrap().unwrap();
    assert_eq!(state.status, TaskStatus::Archived);
    // Deliberate deviation from the file backend: events stay queryable for
    // audit (no gzip-and-delete) — the TaskArchived event is in the log.
    let events = store.read_events(&id).await.unwrap();
    assert!(events
        .iter()
        .any(|e| e.event_type == TaskEventType::TaskArchived));
    cleanup(&store, &project).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn reflection_event_projects_counters() {
    let Some(store) = pg_store().await else {
        return;
    };
    let project = scope("reflect");
    let id = store
        .create_task("u", &project, "g", "", "sess")
        .await
        .unwrap();
    for iter in [4u64, 8] {
        store
            .append_event(
                &id,
                "sess",
                None,
                TaskEventType::ReflectionFired,
                TaskEventPayload::ReflectionFired { iteration: iter },
            )
            .await
            .unwrap();
    }
    let state = store.get_task(&id).await.unwrap().unwrap();
    assert_eq!(state.reflection_count, 2);
    assert_eq!(state.last_reflection_iter, 8); // last wins
    cleanup(&store, &project).await;
}

#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn derive_state_unknown_task_errors() {
    let Some(store) = pg_store().await else {
        return;
    };
    assert!(store.get_task("task_nonexistent").await.unwrap().is_none());
    assert!(store.derive_state("task_nonexistent").await.is_err());
}

// ─── Multi-writer concurrency (the MVS3 selling point) ──────────────────────

/// Ten concurrent appends to the SAME task must not lose events and must
/// leave a fully re-derived brief — the per-task `FOR UPDATE` lock
/// serializes writers, so the last committed brief sees all 10 notes. The
/// file backend cannot guarantee this (index read-modify-write races).
#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn concurrent_appends_same_task() {
    let Some(store) = pg_store().await else {
        return;
    };
    let project = scope("conc-same");
    let id = store
        .create_task("u", &project, "g", "", "sess")
        .await
        .unwrap();
    let store = Arc::new(store);
    let mut handles = Vec::new();
    for i in 0..10 {
        let s = Arc::clone(&store);
        let id = id.clone();
        handles.push(tokio::spawn(async move {
            s.append_event(
                &id,
                "sess",
                None,
                TaskEventType::NoteAdded,
                TaskEventPayload::NoteAdded {
                    note: Note {
                        id: format!("n{i}"),
                        content: format!("note {i}"),
                        ts: String::new(),
                    },
                },
            )
            .await
            .unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    // No lost events: 1 TaskCreated + 10 NoteAdded.
    let events = store.read_events(&id).await.unwrap();
    assert_eq!(events.len(), 11);
    let state = store.get_task(&id).await.unwrap().unwrap();
    assert_eq!(state.notes.len(), 10);
    // The brief was re-derived under the lock — it agrees with the log.
    let open = store.list_open_tasks("u", &project).await.unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].status, TaskStatus::Active);
    // Under the per-task lock, seq order == commit order, so the brief's ts
    // must equal the LAST event in seq order (not merely some event's ts).
    assert_eq!(
        open[0].last_event_ts,
        events.last().unwrap().ts,
        "brief ts must reflect the last committed event"
    );
    cleanup(&store, &project).await;
}

/// Two tasks appending concurrently (different rows → no lock contention)
/// stay isolated: each log and each brief derives only its own events.
#[tokio::test]
#[ignore = "requires ONEAI_TEST_PG_DSN (disposable Postgres)"]
async fn concurrent_two_tasks_isolated() {
    let Some(store) = pg_store().await else {
        return;
    };
    let project = scope("conc-two");
    let a = store
        .create_task("u", &project, "task A", "", "s1")
        .await
        .unwrap();
    let b = store
        .create_task("u", &project, "task B", "", "s2")
        .await
        .unwrap();
    let store = Arc::new(store);
    let (ha, hb) = {
        let (sa, sb) = (Arc::clone(&store), Arc::clone(&store));
        let (a2, b2) = (a.clone(), b.clone());
        (
            tokio::spawn(async move {
                for i in 0..8 {
                    sa.append_event(
                        &a2,
                        "s1",
                        None,
                        TaskEventType::NoteAdded,
                        TaskEventPayload::NoteAdded {
                            note: Note {
                                id: format!("a{i}"),
                                content: format!("A note {i}"),
                                ts: String::new(),
                            },
                        },
                    )
                    .await
                    .unwrap();
                }
            }),
            tokio::spawn(async move {
                for i in 0..8 {
                    sb.append_event(
                        &b2,
                        "s2",
                        None,
                        TaskEventType::NoteAdded,
                        TaskEventPayload::NoteAdded {
                            note: Note {
                                id: format!("b{i}"),
                                content: format!("B note {i}"),
                                ts: String::new(),
                            },
                        },
                    )
                    .await
                    .unwrap();
                }
            }),
        )
    };
    ha.await.unwrap();
    hb.await.unwrap();
    let sa = store.get_task(&a).await.unwrap().unwrap();
    let sb = store.get_task(&b).await.unwrap().unwrap();
    assert_eq!(sa.notes.len(), 8);
    assert_eq!(sb.notes.len(), 8);
    assert!(sa.notes.iter().all(|n| n.content.starts_with("A")));
    assert!(sb.notes.iter().all(|n| n.content.starts_with("B")));
    cleanup(&store, &project).await;
}
