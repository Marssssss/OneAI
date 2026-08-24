//! Permission-decision audit trail (gap-analysis P1 #9).
//!
//! Every tool-permission decision — domain-policy deny / auto-approve,
//! Guardian content-review verdict, human gate approval / abort / revise, or
//! direct execution of a low-risk tool — can be recorded as a structured
//! [`PermissionAuditEvent`] through a [`PermissionAuditLog`] sink.
//!
//! This replaces the previous best-effort `tracing::warn!`-only visibility:
//! an audit log is durable, structured (JSON), and survives log-level
//! filtering. The two dispatch paths that make permission decisions
//! (`oneai_tool::execute_with_approval` for the ToolExecutor /
//! code-interpreter bridge, and `AgentLoop::execute_tool_calls` for the
//! agent loop) both accept an optional `Arc<dyn PermissionAuditLog>`.
//!
//! **Privacy**: events carry a SHA-256 *digest* of the serialized args, not
//! the args themselves — an audit trail must not become a secret leak. The
//! digest is deterministic, so two identical calls correlate across events.
//!
//! Implementations: [`NoopAuditLog`] (default — zero overhead),
//! [`InMemoryAuditLog`] (bounded ring, for tests / in-process inspection),
//! [`JsonlAuditLog`] (append-only JSONL file, one event per line).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::types::{PermissionLevel, RiskLevel};

/// The source component that made (or observed) the permission decision.
pub const SOURCE_TOOL_EXECUTOR: &str = "tool_executor";
pub const SOURCE_CODE_BRIDGE: &str = "code_interpreter_bridge";
pub const SOURCE_AGENT_LOOP: &str = "agent_loop";
pub const SOURCE_STATE_GRAPH: &str = "state_graph_executor";
pub const SOURCE_REACT_AGENT: &str = "react_agent";

/// How a tool call's permission was decided.
///
/// One variant per terminal decision outcome. Intermediate routings (e.g. a
/// `RequireConfirmation` policy forcing the gate) are not recorded on their
/// own — the terminal gate outcome is the auditable fact.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PermissionDecision {
    /// Denied by the domain `PermissionProfile` before any gate
    /// (`deny_by_default` / explicit deny pattern).
    DeniedByPolicy { reason: String },
    /// Auto-approved by the domain `PermissionProfile` — the gate was skipped.
    AutoApprovedByPolicy,
    /// Guardian content review allowed execution without a human prompt.
    GuardianApproved { reason: String },
    /// Guardian content review denied the call (hard safety guard).
    GuardianDenied { reason: String },
    /// Human / interaction gate approved the call.
    ApprovedByGate,
    /// Human / interaction gate approved with rewritten arguments
    /// (`ProceedWith { ReplaceToolArgs }`).
    ApprovedWithModification,
    /// Human / interaction gate aborted (hard deny).
    DeniedByGate { reason: String },
    /// Human / interaction gate rejected with corrective feedback.
    RevisedByGate { feedback: String },
    /// Executed without approval — the resolved risk level did not require
    /// it, or the `ToolApproval` gate point is disabled.
    DirectExecution,
    /// Rejected by the #27 exposure guard — the tool is not
    /// model-dispatchable (Hidden / CodeModeOnly / Deferred without search).
    NotDispatchable { exposure: String },
    /// The interaction gate errored while the call was pending.
    GateError { error: String },
}

/// One audited permission decision.
///
/// Serialized as a single JSON object (one line in [`JsonlAuditLog`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionAuditEvent {
    /// RFC 3339 UTC timestamp of the decision.
    pub timestamp: String,
    /// The tool the decision was about.
    pub tool_name: String,
    /// The effective risk level at decision time (when known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk_level: Option<RiskLevel>,
    /// The effective permission level at decision time (when known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_level: Option<PermissionLevel>,
    /// The terminal decision.
    pub decision: PermissionDecision,
    /// Hex SHA-256 of the canonical JSON serialization of the call args.
    /// Deterministic for identical args; carries no arg content itself.
    pub args_digest: String,
    /// Which component recorded the event (see the `SOURCE_*` constants).
    pub source: String,
}

impl PermissionAuditEvent {
    /// Build an event, computing the args digest and stamping the time.
    pub fn new(
        tool_name: impl Into<String>,
        risk_level: Option<RiskLevel>,
        decision: PermissionDecision,
        args: &serde_json::Value,
        source: impl Into<String>,
    ) -> Self {
        Self {
            timestamp: chrono::Utc::now().to_rfc3339(),
            tool_name: tool_name.into(),
            risk_level,
            permission_level: risk_level.map(PermissionLevel::from_risk_level),
            decision,
            args_digest: args_digest(args),
            source: source.into(),
        }
    }
}

/// Hex-encoded SHA-256 of the canonical JSON serialization of `args`.
pub fn args_digest(args: &serde_json::Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(args.to_string().as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

/// A sink for permission-decision audit events.
///
/// Implementations must be cheap and infallible from the caller's
/// perspective — auditing is a best-effort side channel and must never block
/// or fail the tool dispatch itself.
pub trait PermissionAuditLog: Send + Sync {
    /// Record one decision. Called synchronously on the dispatch path.
    fn record(&self, event: &PermissionAuditEvent);
}

/// Emit an event to an optional audit log (no-op when `None`).
///
/// The call-site ergonomic helper — every decision point funnels through
/// this, so wiring a log is a one-line change per component.
pub fn emit_audit(log: Option<&Arc<dyn PermissionAuditLog>>, event: PermissionAuditEvent) {
    if let Some(l) = log {
        l.record(&event);
    }
}

/// Default sink: drops every event. Zero overhead.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopAuditLog;

impl PermissionAuditLog for NoopAuditLog {
    fn record(&self, _event: &PermissionAuditEvent) {}
}

/// Bounded in-memory ring buffer of events — for tests and in-process
/// inspection (e.g. a future `audit/*` RPC). Oldest events drop at capacity.
#[derive(Debug)]
pub struct InMemoryAuditLog {
    capacity: usize,
    events: Mutex<VecDeque<PermissionAuditEvent>>,
}

impl InMemoryAuditLog {
    /// Create a ring buffer holding at most `capacity` events (min 1).
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            events: Mutex::new(VecDeque::new()),
        }
    }

    /// Snapshot of the buffered events, oldest first.
    pub fn snapshot(&self) -> Vec<PermissionAuditEvent> {
        self.events
            .lock()
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Number of buffered events.
    pub fn len(&self) -> usize {
        self.events.lock().map(|q| q.len()).unwrap_or(0)
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl PermissionAuditLog for InMemoryAuditLog {
    fn record(&self, event: &PermissionAuditEvent) {
        if let Ok(mut q) = self.events.lock() {
            if q.len() >= self.capacity {
                q.pop_front();
            }
            q.push_back(event.clone());
        }
    }
}

/// Append-only JSONL file sink — one serialized event per line.
///
/// Durable audit trail across restarts. Writes are synchronous (one line
/// each) behind a mutex; a write failure is logged via `tracing::warn!` and
/// swallowed — auditing never fails the dispatch.
pub struct JsonlAuditLog {
    path: PathBuf,
    file: Mutex<std::fs::File>,
}

impl JsonlAuditLog {
    /// Open (creating if needed, appending if present) the audit file at
    /// `path`. Parent directories are created on demand.
    pub fn new(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    /// The file this sink appends to.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Debug for JsonlAuditLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonlAuditLog")
            .field("path", &self.path)
            .finish()
    }
}

impl PermissionAuditLog for JsonlAuditLog {
    fn record(&self, event: &PermissionAuditEvent) {
        use std::io::Write;
        let line = match serde_json::to_string(event) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("permission audit: serialize failed: {}", e);
                return;
            }
        };
        match self.file.lock() {
            Ok(mut f) => {
                if let Err(e) = writeln!(f, "{}", line) {
                    tracing::warn!(
                        "permission audit: write to {} failed: {}",
                        self.path.display(),
                        e
                    );
                }
            }
            Err(_) => {
                tracing::warn!("permission audit: file lock poisoned; event dropped");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(tool: &str, decision: PermissionDecision) -> PermissionAuditEvent {
        PermissionAuditEvent::new(
            tool,
            Some(RiskLevel::High),
            decision,
            &json!({"a": 1}),
            SOURCE_AGENT_LOOP,
        )
    }

    #[test]
    fn args_digest_is_deterministic_and_distinct() {
        let a = json!({"command": "ls"});
        let b = json!({"command": "ls"});
        let c = json!({"command": "rm -rf /"});
        assert_eq!(args_digest(&a), args_digest(&b));
        assert_ne!(args_digest(&a), args_digest(&c));
        assert_eq!(args_digest(&a).len(), 64); // hex sha256
    }

    #[test]
    fn event_serializes_to_single_line_json() {
        let e = ev(
            "shell",
            PermissionDecision::DeniedByGate {
                reason: "no".into(),
            },
        );
        let s = serde_json::to_string(&e).unwrap();
        assert!(!s.contains('\n'));
        let round: PermissionAuditEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(round.tool_name, "shell");
        assert_eq!(round.decision, e.decision);
        assert_eq!(round.args_digest, e.args_digest);
        assert_eq!(round.permission_level, Some(PermissionLevel::Full));
    }

    #[test]
    fn in_memory_log_caps_at_capacity() {
        let log = InMemoryAuditLog::new(2);
        log.record(&ev("t1", PermissionDecision::DirectExecution));
        log.record(&ev("t2", PermissionDecision::DirectExecution));
        log.record(&ev("t3", PermissionDecision::DirectExecution));
        assert_eq!(log.len(), 2);
        let snap = log.snapshot();
        assert_eq!(snap[0].tool_name, "t2"); // oldest (t1) dropped
        assert_eq!(snap[1].tool_name, "t3");
    }

    #[test]
    fn jsonl_log_appends_parseable_lines() {
        let dir = std::env::temp_dir().join(format!("oneai-audit-test-{}", std::process::id()));
        let path = dir.join("audit.jsonl");
        let _ = std::fs::remove_file(&path);
        {
            let log = JsonlAuditLog::new(&path).unwrap();
            log.record(&ev("shell", PermissionDecision::ApprovedByGate));
            log.record(&ev(
                "read_file",
                PermissionDecision::DeniedByPolicy { reason: "x".into() },
            ));
        }
        // Re-open (append mode) and write a third — file must accumulate.
        {
            let log = JsonlAuditLog::new(&path).unwrap();
            log.record(&ev("calc", PermissionDecision::DirectExecution));
        }
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in &lines {
            let e: PermissionAuditEvent = serde_json::from_str(line).unwrap();
            assert!(!e.timestamp.is_empty());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn emit_with_none_is_noop() {
        // Must not panic.
        emit_audit(None, ev("x", PermissionDecision::DirectExecution));
    }

    #[test]
    fn emit_routes_to_log() {
        let log = Arc::new(InMemoryAuditLog::new(8));
        let boxed: Option<Arc<dyn PermissionAuditLog>> = Some(log.clone());
        emit_audit(boxed.as_ref(), ev("x", PermissionDecision::DirectExecution));
        assert_eq!(log.len(), 1);
        assert_eq!(log.snapshot()[0].tool_name, "x");
    }
}
