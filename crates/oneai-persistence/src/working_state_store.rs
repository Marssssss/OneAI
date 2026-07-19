//! File-backed working-state store — the durable source of truth for agent
//! working state (goal / steps / decisions / blockers / notes), persisted as
//! an append-only per-task event log independent of any session transcript.
//!
//! ## Storage layout
//! - `<root>/tasks/{task_id}.jsonl` — append-only event log (one JSON object
//!   per line). A `Snapshot` event is a materialized checkpoint *inside* the
//!   log, so state and events cannot drift (§8.4): `derive_state` replays
//!   from the latest `Snapshot` + the tail.
//! - `<root>/tasks.index.json` — lightweight index `{ task_id: TaskBrief }`
//!   so `list_open_tasks` (cross-session discovery) reads one file instead
//!   of deriving every task.
//! - `<root>/tasks/{task_id}.archive.jsonl.gz` — gzipped archived log.
//!
//! ## Crash safety
//! Append-only: a partial final line is skipped on reload (§8.1). The hot
//! read path (per-turn pinned re-injection) uses the in-memory `WorkingState`
//! projection held in `LoopState`, not this store — zero IO per turn.
//!
//! `root` is profile-dependent: CodingPack uses an in-repo `.oneai/` (git =
//! free durability + reconciliation source), assistant uses `~/.oneai/`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use oneai_core::error::{OneAIError, Result};
use oneai_core::traits::WorkingStateStore;
use oneai_core::{
    TaskBrief, TaskEvent, TaskEventPayload, TaskEventType, TaskStatus, TASK_EVENT_SCHEMA_VERSION,
    WorkingState,
};
use tokio::io::AsyncWriteExt;

/// File-backed working-state store.
pub struct FileWorkingStateStore {
    root: PathBuf,
    /// Compaction thresholds. Defaults mirror the CodingPack policy.
    event_threshold: usize,
    keep_recent: usize,
}

impl FileWorkingStateStore {
    /// Create a store rooted at `root`. The `tasks/` subdir and index are
    /// created lazily on first write.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            event_threshold: 200,
            keep_recent: 50,
        }
    }

    /// Override compaction thresholds.
    pub fn with_compaction(mut self, event_threshold: usize, keep_recent: usize) -> Self {
        self.event_threshold = event_threshold;
        self.keep_recent = keep_recent;
        self
    }

    /// The root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn tasks_dir(&self) -> PathBuf {
        self.root.join("tasks")
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("tasks.index.json")
    }

    fn task_log_path(&self, task_id: &str) -> PathBuf {
        self.tasks_dir().join(format!("{}.jsonl", task_id))
    }

    async fn ensure_dirs(&self) -> Result<()> {
        tokio::fs::create_dir_all(self.tasks_dir()).await.map_err(|e| {
            OneAIError::Persistence(format!(
                "Failed to create working-state dir '{}': {}",
                self.tasks_dir().display(),
                e
            ))
        })
    }

    /// Read every event line from a task log, skipping malformed/partial
    /// trailing lines (crash safety §8.1). Returns events in log order.
    async fn read_events(&self, task_id: &str) -> Result<Vec<TaskEvent>> {
        let path = self.task_log_path(task_id);
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(OneAIError::Persistence(format!(
                    "Failed to read task log '{}': {}",
                    path.display(),
                    e
                )))
            }
        };
        let mut events = Vec::new();
        for line in bytes.split(|b| *b == b'\n') {
            if line.trim_ascii().is_empty() {
                continue;
            }
            // A line that fails to deserialize is a partial / corrupt write —
            // skip it rather than aborting the whole log.
            match serde_json::from_slice::<TaskEvent>(line) {
                Ok(ev) => events.push(ev),
                Err(e) => {
                    tracing::warn!(
                        "Skipping malformed task event line in '{}': {}",
                        path.display(),
                        e
                    );
                }
            }
        }
        Ok(events)
    }

    /// Append one event line (newline-terminated). Creates the log + dirs if
    /// missing.
    async fn append_event_line(&self, event: &TaskEvent) -> Result<()> {
        self.ensure_dirs().await?;
        let path = self.task_log_path(&event.task_id);
        let mut line = serde_json::to_vec(event)
            .map_err(|e| OneAIError::Serialization(format!("Failed to encode event: {}", e)))?;
        line.push(b'\n');
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| {
                OneAIError::Persistence(format!(
                    "Failed to open task log '{}': {}",
                    path.display(),
                    e
                ))
            })?;
        file.write_all(&line).await.map_err(|e| {
            OneAIError::Persistence(format!(
                "Failed to append task log '{}': {}",
                path.display(),
                e
            ))
        })?;
        Ok(())
    }

    /// Rewrite the task log with the given events (used by compaction).
    async fn rewrite_events(&self, task_id: &str, events: &[TaskEvent]) -> Result<()> {
        self.ensure_dirs().await?;
        let path = self.task_log_path(task_id);
        let mut buf = Vec::with_capacity(events.len() * 256);
        for ev in events {
            serde_json::to_writer(&mut buf, ev)
                .map_err(|e| OneAIError::Serialization(format!("Failed to encode event: {}", e)))?;
            buf.push(b'\n');
        }
        tokio::fs::write(&path, &buf).await.map_err(|e| {
            OneAIError::Persistence(format!(
                "Failed to rewrite task log '{}': {}",
                path.display(),
                e
            ))
        })?;
        Ok(())
    }

    // ─── Index (tasks.index.json) ──────────────────────────────────────────

    async fn read_index(&self) -> Result<HashMap<String, TaskBrief>> {
        let path = self.index_path();
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(e) => {
                return Err(OneAIError::Persistence(format!(
                    "Failed to read index '{}': {}",
                    path.display(),
                    e
                )))
            }
        };
        if bytes.trim_ascii().is_empty() {
            return Ok(HashMap::new());
        }
        // A corrupt index is non-fatal — rebuild from logs.
        match serde_json::from_slice::<HashMap<String, TaskBrief>>(&bytes) {
            Ok(m) => Ok(m),
            Err(e) => {
                tracing::warn!("Corrupt working-state index, will rebuild: {}", e);
                Ok(HashMap::new())
            }
        }
    }

    async fn write_index(&self, index: &HashMap<String, TaskBrief>) -> Result<()> {
        self.ensure_dirs().await?;
        let path = self.index_path();
        let json = serde_json::to_vec_pretty(index)
            .map_err(|e| OneAIError::Serialization(format!("Failed to encode index: {}", e)))?;
        // Atomic-ish: write to temp then rename.
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, &json).await.map_err(|e| {
            OneAIError::Persistence(format!("Failed to write index tmp '{}': {}", tmp.display(), e))
        })?;
        tokio::fs::rename(&tmp, &path).await.map_err(|e| {
            OneAIError::Persistence(format!("Failed to rename index '{}': {}", path.display(), e))
        })?;
        Ok(())
    }

    async fn upsert_index_entry(&self, brief: TaskBrief) -> Result<()> {
        let mut index = self.read_index().await?;
        index.insert(brief.task_id.clone(), brief);
        self.write_index(&index).await
    }
}

// ─── Projector: derive WorkingState from events ──────────────────────────────

/// Replay events onto a `WorkingState`. `Snapshot` events replace state
/// wholesale (then subsequent events apply on top); all other events mutate.
///
/// Exposed as a free function so tests can call it without a store instance.
pub fn project(events: &[TaskEvent]) -> Option<WorkingState> {
    let mut state: Option<WorkingState> = None;
    for ev in events {
        apply_event(&mut state, ev);
    }
    state
}

fn apply_event(state: &mut Option<WorkingState>, ev: &TaskEvent) {
    // ts / metadata carried on every event for last-updated tracking.
    match (&ev.event_type, &ev.payload) {
        (TaskEventType::Snapshot, TaskEventPayload::Snapshot { state: snap }) => {
            // Materialized checkpoint — replace wholesale, then continue.
            let mut s = snap.clone();
            // Keep the live task_id/user_id/project in sync with the log.
            if s.task_id.is_empty() {
                s.task_id = ev.task_id.clone();
            }
            *state = Some(s);
        }
        (TaskEventType::TaskCreated, TaskEventPayload::Task { goal, intent }) => {
            *state = Some(WorkingState {
                task_id: ev.task_id.clone(),
                user_id: ev.session_id.clone(), // corrected below via index; see create_task
                project: String::new(),
                goal: goal.clone(),
                intent: intent.clone(),
                status: TaskStatus::Active,
                steps: Vec::new(),
                decisions: Vec::new(),
                blockers: Vec::new(),
                notes: Vec::new(),
                owner_session: ev.session_id.clone(),
                created_at: ev.ts.clone(),
                updated_at: ev.ts.clone(),
            });
        }
        (TaskEventType::GoalRevised, TaskEventPayload::Task { goal, intent }) => {
            if let Some(s) = state.as_mut() {
                s.goal = goal.clone();
                if !intent.is_empty() {
                    s.intent = intent.clone();
                }
                s.updated_at = ev.ts.clone();
            }
        }
        (TaskEventType::StepAdded, TaskEventPayload::StepAdded { step }) => {
            if let Some(s) = state.as_mut() {
                if let Some(existing) = s.steps.iter_mut().find(|x| x.id == step.id) {
                    *existing = step.clone();
                } else {
                    s.steps.push(step.clone());
                }
                s.updated_at = ev.ts.clone();
            }
        }
        (TaskEventType::StepStatusChanged, TaskEventPayload::StepStatusChanged {
            step_id, status, active_form,
        }) => {
            if let Some(s) = state.as_mut() {
                if let Some(step) = s.steps.iter_mut().find(|x| x.id == *step_id) {
                    step.status = *status;
                    if let Some(af) = active_form {
                        step.active_form = Some(af.clone());
                    }
                    step.updated_at = ev.ts.clone();
                }
                s.updated_at = ev.ts.clone();
            }
        }
        (TaskEventType::DecisionMade, TaskEventPayload::DecisionMade { decision }) => {
            if let Some(s) = state.as_mut() {
                if let Some(existing) = s.decisions.iter_mut().find(|x| x.id == decision.id) {
                    *existing = decision.clone();
                } else {
                    s.decisions.push(decision.clone());
                }
                s.updated_at = ev.ts.clone();
            }
        }
        (TaskEventType::BlockerRaised, TaskEventPayload::BlockerRaised { blocker }) => {
            if let Some(s) = state.as_mut() {
                if let Some(existing) = s.blockers.iter_mut().find(|x| x.id == blocker.id) {
                    *existing = blocker.clone();
                } else {
                    s.blockers.push(blocker.clone());
                }
                s.updated_at = ev.ts.clone();
            }
        }
        (TaskEventType::BlockerResolved, TaskEventPayload::BlockerResolved {
            blocker_id, resolution,
        }) => {
            if let Some(s) = state.as_mut() {
                if let Some(b) = s.blockers.iter_mut().find(|x| x.id == *blocker_id) {
                    b.status = oneai_core::BlockerStatus::Resolved;
                    b.resolution = Some(resolution.clone());
                }
                s.updated_at = ev.ts.clone();
            }
        }
        (TaskEventType::NoteAdded, TaskEventPayload::NoteAdded { note }) => {
            if let Some(s) = state.as_mut() {
                s.notes.push(note.clone());
                s.updated_at = ev.ts.clone();
            }
        }
        (TaskEventType::TaskPaused, _) => {
            if let Some(s) = state.as_mut() {
                s.status = TaskStatus::Paused;
                s.updated_at = ev.ts.clone();
            }
        }
        (TaskEventType::TaskResumed, _) => {
            if let Some(s) = state.as_mut() {
                s.status = TaskStatus::Active;
                s.updated_at = ev.ts.clone();
            }
        }
        (TaskEventType::TaskCompleted, _) => {
            if let Some(s) = state.as_mut() {
                s.status = TaskStatus::Completed;
                s.updated_at = ev.ts.clone();
            }
        }
        (TaskEventType::TaskArchived, _) => {
            if let Some(s) = state.as_mut() {
                s.status = TaskStatus::Archived;
                s.updated_at = ev.ts.clone();
            }
        }
        (TaskEventType::Reconciliation, _) => {
            // Reconciliation is informational; it doesn't mutate working state,
            // only flags drift (the caller surfaces it via the pinned block).
            if let Some(s) = state.as_mut() {
                s.updated_at = ev.ts.clone();
            }
        }
        _ => {
            // Unknown / mismatched event-type+payload pair — log and skip.
            tracing::warn!(
                "Working-state event {} has mismatched type {:?} / payload, skipping",
                ev.id,
                ev.event_type
            );
        }
    }
}

/// Count open (non-completed) steps.
fn open_step_count(state: &WorkingState) -> u32 {
    state
        .steps
        .iter()
        .filter(|s| !matches!(s.status, oneai_core::StepStatus::Completed))
        .count() as u32
}

/// Count open blockers.
fn open_blocker_count(state: &WorkingState) -> u32 {
    state
        .blockers
        .iter()
        .filter(|b| matches!(b.status, oneai_core::BlockerStatus::Open))
        .count() as u32
}

#[async_trait]
impl WorkingStateStore for FileWorkingStateStore {
    async fn create_task(
        &self,
        user_id: &str,
        project: &str,
        goal: &str,
        intent: &str,
        session_id: &str,
    ) -> Result<String> {
        let task_id = format!("task_{}", uuid::Uuid::new_v4().simple());
        let ev = TaskEvent {
            id: uuid::Uuid::new_v4().to_string(),
            task_id: task_id.clone(),
            session_id: session_id.to_string(),
            parent_event_id: None,
            event_type: TaskEventType::TaskCreated,
            payload: TaskEventPayload::Task {
                goal: goal.to_string(),
                intent: intent.to_string(),
            },
            schema_version: TASK_EVENT_SCHEMA_VERSION,
            ts: now_rfc3339(),
        };
        self.append_event_line(&ev).await?;
        // The TaskCreated event records session_id but the durable user/project
        // scoping lives in the index (events don't all carry user/project).
        let brief = TaskBrief {
            task_id: task_id.clone(),
            goal: goal.to_string(),
            status: TaskStatus::Active,
            open_step_count: 0,
            open_blocker_count: 0,
            user_id: user_id.to_string(),
            project: project.to_string(),
            last_event_ts: ev.ts.clone(),
            file: format!("tasks/{}.jsonl", task_id),
        };
        self.upsert_index_entry(brief).await?;
        Ok(task_id)
    }

    async fn get_task(&self, task_id: &str) -> Result<Option<WorkingState>> {
        let events = self.read_events(task_id).await?;
        if events.is_empty() {
            return Ok(None);
        }
        let mut state = match project(&events) {
            Some(s) => s,
            None => return Ok(None),
        };
        // Backfill user_id / project from the index (events don't carry them).
        let index = self.read_index().await?;
        if let Some(brief) = index.get(task_id) {
            state.user_id = brief.user_id.clone();
            state.project = brief.project.clone();
            if state.owner_session.is_empty() {
                state.owner_session = brief.user_id.clone();
            }
        }
        Ok(Some(state))
    }

    async fn list_open_tasks(
        &self,
        user_id: &str,
        project: &str,
    ) -> Result<Vec<TaskBrief>> {
        let index = self.read_index().await?;
        let mut briefs: Vec<TaskBrief> = index
            .into_values()
            .filter(|b| {
                b.status.is_open()
                    && (user_id.is_empty() || b.user_id == user_id)
                    && (project.is_empty() || b.project == project)
            })
            .collect();
        // Most-recently-touched first.
        briefs.sort_by(|a, b| b.last_event_ts.cmp(&a.last_event_ts));
        Ok(briefs)
    }

    async fn append_event(
        &self,
        task_id: &str,
        session_id: &str,
        parent_event_id: Option<&str>,
        event_type: TaskEventType,
        payload: TaskEventPayload,
    ) -> Result<String> {
        let ev = TaskEvent {
            id: uuid::Uuid::new_v4().to_string(),
            task_id: task_id.to_string(),
            session_id: session_id.to_string(),
            parent_event_id: parent_event_id.map(|s| s.to_string()),
            event_type,
            payload,
            schema_version: TASK_EVENT_SCHEMA_VERSION,
            ts: now_rfc3339(),
        };
        self.append_event_line(&ev).await?;

        // Update the index entry's status / counts / ts.
        let mut index = self.read_index().await?;
        let brief = index.entry(task_id.to_string()).or_insert_with(|| TaskBrief {
            task_id: task_id.to_string(),
            goal: String::new(),
            status: TaskStatus::Active,
            open_step_count: 0,
            open_blocker_count: 0,
            user_id: String::new(),
            project: String::new(),
            last_event_ts: ev.ts.clone(),
            file: format!("tasks/{}.jsonl", task_id),
        });
        brief.last_event_ts = ev.ts.clone();

        // Derive counts/status cheaply from current state (re-derive on demand
        // is acceptable here — append is not the hot path).
        if let Ok(Some(state)) = self.get_task(task_id).await {
            brief.status = state.status;
            brief.open_step_count = open_step_count(&state);
            brief.open_blocker_count = open_blocker_count(&state);
            if brief.goal.is_empty() {
                brief.goal = state.goal;
            }
        }
        self.write_index(&index).await?;
        Ok(ev.id)
    }

    async fn derive_state(&self, task_id: &str) -> Result<WorkingState> {
        match self.get_task(task_id).await? {
            Some(s) => Ok(s),
            None => Err(OneAIError::Persistence(format!(
                "No working state for task '{}'",
                task_id
            ))),
        }
    }

    async fn compact_if_needed(&self, task_id: &str) -> Result<()> {
        let events = self.read_events(task_id).await?;
        if events.len() < self.event_threshold {
            return Ok(());
        }
        // Snapshot = state projected from events[..tail_start]; keep events
        // [tail_start..] as the live tail. derive_state then replays the
        // snapshot + tail = final state, with no double-application.
        let tail_start = events.len().saturating_sub(self.keep_recent);
        let snapshot_state = match project(&events[..tail_start]) {
            Some(s) => s,
            None => return Ok(()),
        };
        let snapshot_event = TaskEvent {
            id: uuid::Uuid::new_v4().to_string(),
            task_id: task_id.to_string(),
            session_id: snapshot_state.owner_session.clone(),
            parent_event_id: events.get(tail_start.saturating_sub(1)).map(|e| e.id.clone()),
            event_type: TaskEventType::Snapshot,
            payload: TaskEventPayload::Snapshot {
                state: snapshot_state,
            },
            schema_version: TASK_EVENT_SCHEMA_VERSION,
            ts: now_rfc3339(),
        };
        let mut compacted = Vec::with_capacity(1 + (events.len() - tail_start));
        compacted.push(snapshot_event);
        compacted.extend_from_slice(&events[tail_start..]);
        self.rewrite_events(task_id, &compacted).await?;
        tracing::info!(
            "Compacted task '{}' log: {} -> {} events",
            task_id,
            events.len(),
            compacted.len()
        );
        Ok(())
    }

    async fn archive_task(&self, task_id: &str) -> Result<()> {
        // Mark archived first (event + index), then gzip the log.
        let _ = self
            .append_event(
                task_id,
                "",
                None,
                TaskEventType::TaskArchived,
                TaskEventPayload::TaskStatus {},
            )
            .await?;
        let log_path = self.task_log_path(task_id);
        let archive_path = self
            .tasks_dir()
            .join(format!("{}.archive.jsonl.gz", task_id));
        if tokio::fs::try_exists(&log_path).await.unwrap_or(false) {
            let bytes = tokio::fs::read(&log_path).await.map_err(|e| {
                OneAIError::Persistence(format!("Failed to read log for archive: {}", e))
            })?;
            // Gzip in one pass via std::io::Write (sync is fine — a final
            // archival of a bounded log).
            use std::io::Write;
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&bytes).map_err(|e| {
                OneAIError::Persistence(format!("Failed to write gzip stream: {}", e))
            })?;
            let compressed = encoder
                .finish()
                .map_err(|e| OneAIError::Persistence(format!("Failed to finish gzip stream: {}", e)))?;
            tokio::fs::write(&archive_path, &compressed).await.map_err(|e| {
                OneAIError::Persistence(format!("Failed to write archive: {}", e))
            })?;
            let _ = tokio::fs::remove_file(&log_path).await;
        }
        // Update index file pointer + status.
        let mut index = self.read_index().await?;
        if let Some(brief) = index.get_mut(task_id) {
            brief.status = TaskStatus::Archived;
            brief.file = format!("tasks/{}.archive.jsonl.gz", task_id);
        }
        self.write_index(&index).await?;
        Ok(())
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::{Blocker, BlockerStatus, Decision, Note, Step, StepStatus};
    use tempfile::TempDir;

    fn store() -> (TempDir, FileWorkingStateStore) {
        let dir = TempDir::new().unwrap();
        let s = FileWorkingStateStore::new(dir.path().to_path_buf()).with_compaction(5, 2);
        (dir, s)
    }

    #[tokio::test]
    async fn create_and_get_task() {
        let (_d, s) = store();
        let id = s
            .create_task("alice", "proj", "refactor auth", "split into services", "sess1")
            .await
            .unwrap();
        let state = s.get_task(&id).await.unwrap().unwrap();
        assert_eq!(state.goal, "refactor auth");
        assert_eq!(state.intent, "split into services");
        assert_eq!(state.status, TaskStatus::Active);
        assert!(state.steps.is_empty());
    }

    #[tokio::test]
    async fn step_lifecycle_projects() {
        let (_d, s) = store();
        let id = s.create_task("u", "p", "g", "", "sess").await.unwrap();
        s.append_event(
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
        s.append_event(
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
        let state = s.get_task(&id).await.unwrap().unwrap();
        assert_eq!(state.steps.len(), 1);
        assert_eq!(state.steps[0].status, StepStatus::InProgress);
        assert_eq!(state.steps[0].active_form.as_deref(), Some("writing tests"));
    }

    #[tokio::test]
    async fn decisions_and_blockers_project() {
        let (_d, s) = store();
        let id = s.create_task("u", "p", "g", "", "sess").await.unwrap();
        s.append_event(
            &id, "sess", None,
            TaskEventType::DecisionMade,
            TaskEventPayload::DecisionMade {
                decision: Decision {
                    id: "d1".into(),
                    question: "ORM?".into(),
                    chosen: "sqlx".into(),
                    rationale: "async".into(),
                    alternatives: vec!["diesel".into()],
                    step_id: None,
                    ts: String::new(),
                },
            },
        ).await.unwrap();
        s.append_event(
            &id, "sess", None,
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
        ).await.unwrap();
        s.append_event(
            &id, "sess", None,
            TaskEventType::BlockerResolved,
            TaskEventPayload::BlockerResolved {
                blocker_id: "b1".into(),
                resolution: "retried".into(),
            },
        ).await.unwrap();
        let state = s.get_task(&id).await.unwrap().unwrap();
        assert_eq!(state.decisions.len(), 1);
        assert_eq!(state.decisions[0].chosen, "sqlx");
        assert_eq!(state.blockers.len(), 1);
        assert_eq!(state.blockers[0].status, BlockerStatus::Resolved);
    }

    #[tokio::test]
    async fn list_open_tasks_cross_session() {
        let (_d, s) = store();
        let a = s.create_task("alice", "proj", "task A", "", "s1").await.unwrap();
        let _b = s.create_task("bob", "proj", "task B", "", "s2").await.unwrap();
        let c = s.create_task("alice", "proj", "task C", "", "s3").await.unwrap();
        // Complete C — should drop out of open list.
        s.append_event(&c, "s3", None, TaskEventType::TaskCompleted, TaskEventPayload::TaskStatus {})
            .await
            .unwrap();
        let open = s.list_open_tasks("alice", "proj").await.unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].goal, "task A");
        assert_eq!(open[0].task_id, a);
    }

    #[tokio::test]
    async fn compaction_preserves_derived_state() {
        let (_d, s) = store(); // threshold 5, keep_recent 2
        let id = s.create_task("u", "p", "g", "", "sess").await.unwrap();
        for i in 0..6 {
            s.append_event(
                &id, "sess", None,
                TaskEventType::NoteAdded,
                TaskEventPayload::NoteAdded {
                    note: Note { id: format!("n{i}"), content: format!("note {i}"), ts: String::new() },
                },
            ).await.unwrap();
        }
        s.compact_if_needed(&id).await.unwrap();
        // After compaction: 1 snapshot + 2 tail = 3 events; derived state must
        // still have all 6 notes (they're folded into the snapshot).
        let state = s.get_task(&id).await.unwrap().unwrap();
        assert_eq!(state.notes.len(), 6);
        let events = s.read_events(&id).await.unwrap();
        assert!(events.len() <= 3);
        // First event must be the snapshot.
        assert_eq!(events[0].event_type, TaskEventType::Snapshot);
    }

    #[tokio::test]
    async fn partial_final_line_is_ignored() {
        let (_dir, s) = store();
        let id = s.create_task("u", "p", "g", "", "sess").await.unwrap();
        // Corrupt the log by appending a truncated line.
        let path = s.task_log_path(&id);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        writeln!(file, "{{ broken json").unwrap();
        drop(file);
        // get_task must still succeed (corrupt line skipped).
        let state = s.get_task(&id).await.unwrap().unwrap();
        assert_eq!(state.goal, "g");
    }

    #[tokio::test]
    async fn archive_gzips_and_marks_index() {
        let (_d, s) = store();
        let id = s.create_task("u", "p", "g", "", "sess").await.unwrap();
        s.archive_task(&id).await.unwrap();
        let open = s.list_open_tasks("u", "p").await.unwrap();
        assert!(open.is_empty());
        let archive = s.tasks_dir().join(format!("{}.archive.jsonl.gz", id));
        assert!(archive.exists());
    }
}
