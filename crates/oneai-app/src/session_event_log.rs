//! Session event log tap — persists trajectory-relevant `EngineYield`s from the
//! engine bus into a [`SessionEventStore`] so a historical session can rebuild
//! its execution trajectory (issue #40).
//!
//! The live stream is broadcast-only: a frontend that connects mid-session, or
//! loads a saved session, sees only the message transcript. This tap subscribes
//! to the bus, tracks the current session id off the session-lifecycle yields,
//! and appends every whitelisted event as one JSON line under that session.
//!
//! Wire discipline:
//! - **Whitelist, not mirror** — high-volume content events (`stream_chunk` /
//!   `thinking` / `direct_answer`) stay out: their content already lives in the
//!   persisted conversation, and replaying them would double-render chat nodes.
//! - **Size cap** — a single oversized line (a huge tool result) is skipped
//!   with a warning rather than ballooning the log.
//! - **Lag tolerance** — a lagged broadcast receiver logs and continues; the
//!   log is best-effort, never a turn-critical path.

use std::sync::Arc;

use oneai_bus::{EngineBus, EngineYield, InProcessBus};
use oneai_core::traits::SessionEventStore;

/// Maximum serialized size of one persisted event line (larger ones skipped).
pub const MAX_EVENT_LINE_BYTES: usize = 200 * 1024;

/// Whether this yield kind is persisted to the session event log.
///
/// Everything a trajectory timeline needs (turn/iteration boundaries, context
/// assembly, tool calls + results, plan/paradigm changes, delegation lifecycle
/// incl. progress, working-state snapshots, approvals, errors, usage); nothing
/// whose content the conversation transcript already carries.
pub fn is_trajectory_kind(y: &EngineYield) -> bool {
    matches!(
        y,
        EngineYield::TurnStart { .. }
            | EngineYield::IterationStart { .. }
            | EngineYield::ContextAssembled { .. }
            | EngineYield::ContextAccounting { .. }
            | EngineYield::Inference { .. }
            | EngineYield::TokenUsage { .. }
            | EngineYield::ToolCalls { .. }
            | EngineYield::ToolResult { .. }
            | EngineYield::PlanUpdate { .. }
            | EngineYield::ParadigmSwitch { .. }
            | EngineYield::Delegate { .. }
            | EngineYield::DelegateProgress { .. }
            | EngineYield::DelegateComplete { .. }
            | EngineYield::WorkingState { .. }
            | EngineYield::ToolsAdded { .. }
            | EngineYield::ApprovalRequest { .. }
            | EngineYield::Interrupted { .. }
            | EngineYield::Reflection { .. }
            | EngineYield::SpeakerTurn { .. }
            | EngineYield::Error { .. }
            | EngineYield::TurnComplete { .. }
    )
}

/// Spawn the tap task. Returns the handle (mostly for tests).
///
/// Session binding: `session_created` / `session_loaded` / `session_cleared`
/// update the current id *before* any further event is attributed; events
/// arriving before the first binding have no session to belong to and are
/// dropped. `session_ended` stops the tap.
pub fn spawn_session_event_tap(
    bus: Arc<InProcessBus>,
    store: Arc<dyn SessionEventStore>,
) -> tokio::task::JoinHandle<()> {
    use tokio::sync::broadcast::error::RecvError;
    // Subscribe synchronously (not inside the spawned future) so no yield
    // emitted between tap setup and the task's first poll is missed.
    let mut rx = bus.subscribe_yields();
    tokio::spawn(async move {
        let mut current: Option<String> = None;
        loop {
            let y = match rx.recv().await {
                Ok(y) => y,
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!("session event tap lagged; dropped {n} yields");
                    continue;
                }
                Err(RecvError::Closed) => break,
            };
            match &y {
                EngineYield::SessionCreated { id }
                | EngineYield::SessionLoaded { id, .. }
                | EngineYield::SessionCleared { id } => {
                    current = Some(id.clone());
                    continue; // lifecycle markers are not trajectory entries
                }
                EngineYield::SessionDeleted { id } => {
                    if current.as_deref() == Some(id) {
                        current = None;
                    }
                    continue;
                }
                EngineYield::SessionEnded => break,
                _ => {}
            }
            if !is_trajectory_kind(&y) {
                continue;
            }
            let Some(session_id) = current.clone() else {
                continue;
            };
            // Serialize + inject `ts` (epoch ms at persistence time). Live
            // yields carry no timestamp — a frontend derives one from arrival
            // time, which is fine live but useless for a replayed historical
            // session (every event would arrive "now"). The persisted `ts`
            // restores the real timeline on replay.
            match serde_json::to_value(&y) {
                Ok(mut value) => {
                    if let Some(obj) = value.as_object_mut() {
                        obj.insert(
                            "ts".to_string(),
                            serde_json::json!(chrono::Utc::now().timestamp_millis()),
                        );
                    }
                    let line = serde_json::to_string(&value).unwrap_or_default();
                    if line.len() > MAX_EVENT_LINE_BYTES {
                        tracing::warn!(
                            session = %session_id,
                            bytes = line.len(),
                            "skipping oversized session event (> {MAX_EVENT_LINE_BYTES} bytes)"
                        );
                    } else if let Err(e) = store.append(&session_id, &line).await {
                        tracing::warn!(session = %session_id, "session event append failed: {e}");
                    }
                }
                Err(e) => {
                    tracing::warn!(session = %session_id, "session event serialize failed: {e}");
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_bus::{BusParadigmKind, BusToolCall, BusUsageRecord};
    use std::sync::Mutex;

    /// In-memory store for tap tests.
    #[derive(Default)]
    struct MemoryEventStore {
        lines: Mutex<Vec<(String, String)>>,
    }

    #[async_trait::async_trait]
    impl SessionEventStore for MemoryEventStore {
        async fn append(&self, session_id: &str, line: &str) -> oneai_core::error::Result<()> {
            self.lines
                .lock()
                .unwrap()
                .push((session_id.to_string(), line.to_string()));
            Ok(())
        }
        async fn load(&self, session_id: &str) -> oneai_core::error::Result<Vec<String>> {
            Ok(self
                .lines
                .lock()
                .unwrap()
                .iter()
                .filter(|(id, _)| id == session_id)
                .map(|(_, l)| l.clone())
                .collect())
        }
    }

    fn kinds_of(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .filter_map(|l| {
                serde_json::from_str::<serde_json::Value>(l)
                    .ok()
                    .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(String::from))
            })
            .collect()
    }

    #[tokio::test]
    async fn tap_whitelists_and_attributes_to_current_session() {
        let (bus, _rx) = InProcessBus::new();
        let bus = Arc::new(bus);
        let store = Arc::new(MemoryEventStore::default());
        let handle = spawn_session_event_tap(bus.clone(), store.clone());

        // Events before any session binding are dropped.
        bus.emit(EngineYield::TurnStart {
            turn_id: "t0".into(),
            task: "orphan".into(),
        })
        .unwrap();

        bus.emit(EngineYield::SessionCreated { id: "s1".into() })
            .unwrap();
        bus.emit(EngineYield::TurnStart {
            turn_id: "t1".into(),
            task: "hi".into(),
        })
        .unwrap();
        // Non-whitelisted: streaming content stays out of the log.
        bus.emit(EngineYield::StreamChunk {
            turn_id: "t1".into(),
            text: "tok".into(),
            speaker: None,
        })
        .unwrap();
        bus.emit(EngineYield::IterationStart {
            turn_id: "t1".into(),
            iteration: 1,
            paradigm: BusParadigmKind::ReAct,
        })
        .unwrap();
        bus.emit(EngineYield::ToolCalls {
            turn_id: "t1".into(),
            calls: vec![BusToolCall {
                id: "c1".into(),
                name: "shell".into(),
                args: serde_json::json!({}),
            }],
            speaker: None,
        })
        .unwrap();
        bus.emit(EngineYield::TokenUsage {
            usage: BusUsageRecord::default(),
        })
        .unwrap();
        bus.emit(EngineYield::TurnComplete {
            turn_id: "t1".into(),
            summary: oneai_bus::BusTurnSummary {
                final_answer: "done".into(),
                iterations: 1,
                completed: true,
                active_paradigm: BusParadigmKind::ReAct,
            },
        })
        .unwrap();

        // Yield to let the tap drain the broadcast queue.
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let lines = store.load("s1").await.unwrap();
        let kinds = kinds_of(&lines);
        assert_eq!(
            kinds,
            vec![
                "turn_start",
                "iteration_start",
                "tool_calls",
                "token_usage",
                "turn_complete"
            ],
            "stream_chunk must be filtered; lifecycle markers not persisted"
        );
        // Persisted lines carry an engine `ts` (epoch ms) so a historical
        // replay can reconstruct the real timeline rather than "all now".
        assert!(lines[0].contains("\"ts\""), "missing ts in: {}", lines[0]);

        // Session switch: new events attribute to s2.
        bus.emit(EngineYield::SessionLoaded {
            id: "s2".into(),
            messages: vec![],
        })
        .unwrap();
        bus.emit(EngineYield::TurnStart {
            turn_id: "t2".into(),
            task: "second".into(),
        })
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(store.load("s2").await.unwrap().len(), 1);
        assert_eq!(store.load("s1").await.unwrap().len(), 5);

        bus.emit(EngineYield::SessionEnded).unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
    }

    #[test]
    fn whitelist_covers_trajectory_kinds_and_excludes_content() {
        assert!(is_trajectory_kind(&EngineYield::IterationStart {
            turn_id: "t".into(),
            iteration: 1,
            paradigm: BusParadigmKind::ReAct,
        }));
        assert!(is_trajectory_kind(&EngineYield::ContextAssembled {
            turn_id: "t".into(),
            iteration: 1,
            sections: vec![],
            duration_ms: 0,
        }));
        assert!(!is_trajectory_kind(&EngineYield::StreamChunk {
            turn_id: "t".into(),
            text: "x".into(),
            speaker: None,
        }));
        assert!(!is_trajectory_kind(&EngineYield::Thinking {
            turn_id: "t".into(),
            text: "x".into(),
            speaker: None,
        }));
        // Tool intent is a live streaming hint (like stream_chunk/thinking),
        // not a trajectory entry — the assembled call is persisted as ToolCalls.
        assert!(!is_trajectory_kind(&EngineYield::ToolIntent {
            turn_id: "t".into(),
            call_id: "c".into(),
            tool_name: "n".into(),
            speaker: None,
        }));
        assert!(!is_trajectory_kind(&EngineYield::DirectAnswer {
            turn_id: "t".into(),
            text: "x".into(),
            speaker: None,
        }));
        assert!(!is_trajectory_kind(&EngineYield::SessionCreated {
            id: "s".into()
        }));
    }
}
