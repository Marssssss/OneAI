//! Plan state — the live, mutable task list tracked during agent execution.
//!
//! Unlike [`crate::PlanAgent`] which *produces* a plan once, `PlanState` is the
//! in-flight checklist the model mutates during execution via the
//! `task_create` / `task_update` / `task_list` control tools. It lives in
//! [`crate::LoopState`] (agent-side, not model output) so the TUI can render a
//! persistent progress panel and resume correctly after an interrupt.
//!
//! `revision` bumps on every mutation so observers can cheaply detect changes.

use oneai_core::ToolOutput;

use crate::plan_agent::{PlanStep, PlanStepStatus};

/// The live plan tracked across an agent run.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PlanState {
    /// Ordered list of steps. IDs are stable across status updates.
    pub steps: Vec<PlanStep>,
    /// Monotonic counter incremented on every mutation — lets the TUI diff.
    #[serde(default)]
    pub revision: u64,
}

impl PlanState {
    /// Create an empty plan.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace all steps (used by `task_create` / `exit_plan_mode`).
    pub fn set_steps(&mut self, steps: Vec<PlanStep>) {
        self.steps = steps;
        self.bump();
    }

    /// Flip a step's status by id. Returns the step index, or None if not found.
    pub fn set_status(&mut self, id: &str, status: PlanStepStatus) -> Option<usize> {
        let idx = self.steps.iter().position(|s| s.id == id)?;
        self.steps[idx].status = status;
        self.bump();
        Some(idx)
    }

    /// Set the active_form (present-continuous label) for a step.
    pub fn set_active_form(&mut self, id: &str, active_form: Option<String>) -> Option<usize> {
        let idx = self.steps.iter().position(|s| s.id == id)?;
        self.steps[idx].active_form = active_form;
        self.bump();
        Some(idx)
    }

    /// Count steps by status.
    pub fn count_by_status(&self, status: PlanStepStatus) -> usize {
        self.steps.iter().filter(|s| s.status == status).count()
    }

    /// Whether every step is either completed or failed.
    pub fn all_done(&self) -> bool {
        !self.steps.is_empty()
            && self.steps.iter().all(|s| {
                s.status == PlanStepStatus::Completed || s.status == PlanStepStatus::Failed
            })
    }

    fn bump(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
}

// ─── Control-tool handling ────────────────────────────────────────────────────
//
// These tools are intercepted by the AgentLoop (they never reach the tool
// registry): the model calls `task_create`/`task_update`/`task_list` and the
// loop applies the side effect to `LoopState.plan_state` directly, then fires
// `on_plan_update` so the TUI re-renders the plan panel.

/// Tool name for creating/initializing the task list.
pub const TOOL_TASK_CREATE: &str = "task_create";
/// Tool name for updating a step's status.
pub const TOOL_TASK_UPDATE: &str = "task_update";
/// Tool name for listing the current plan.
pub const TOOL_TASK_LIST: &str = "task_list";
/// Tool name for submitting a plan and exiting plan mode (handled in Phase 3).
pub const TOOL_EXIT_PLAN_MODE: &str = "exit_plan_mode";

/// Whether a tool name is a plan/task control tool that the loop intercepts.
pub fn is_control_tool(name: &str) -> bool {
    matches!(
        name,
        TOOL_TASK_CREATE | TOOL_TASK_UPDATE | TOOL_TASK_LIST | TOOL_EXIT_PLAN_MODE
    )
}

/// JSON-schema tool definitions injected into the inference request so the
/// model can call the control tools. Returns the definitions for the given
/// tool name (or all four if you iterate the slice).
pub fn control_tool_definitions() -> Vec<oneai_core::ToolDefinition> {
    vec![
        oneai_core::ToolDefinition {
            name: TOOL_TASK_CREATE.into(),
            description: "Create the task list for the current work. Replaces any existing list. \
                Call this once at the start (or after planning) to commit to a step-by-step plan. \
                Each step needs an id (e.g. \"1\"), a description, and optionally active_form \
                (present-continuous label like \"Running tests\").".into(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "steps": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "description": { "type": "string" },
                                "active_form": { "type": "string" }
                            },
                            "required": ["id", "description"]
                        }
                    }
                },
                "required": ["steps"]
            }),
        },
        oneai_core::ToolDefinition {
            name: TOOL_TASK_UPDATE.into(),
            description: "Update a task step's status. Use status \"in_progress\" when you start a \
                step, \"completed\" when it succeeds, \"failed\" when it cannot be done. Keep exactly \
                one step in_progress at a time.".into(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "description": "The step id to update." },
                    "status": { "type": "string", "enum": ["pending", "in_progress", "completed", "failed"] },
                    "active_form": { "type": "string", "description": "Optional new present-continuous label." }
                },
                "required": ["task_id", "status"]
            }),
        },
        oneai_core::ToolDefinition {
            name: TOOL_TASK_LIST.into(),
            description: "List the current task plan with each step's status. Read-only.".into(),
            parameters_schema: serde_json::json!({ "type": "object", "properties": {} }),
        },
        oneai_core::ToolDefinition {
            name: TOOL_EXIT_PLAN_MODE.into(),
            description: "Submit the plan and exit plan mode to begin execution. Call this (instead \
                of writing tools) when you have finished researching and are ready to present the \
                plan for approval. `steps` become the tracked task list.".into(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "plan": { "type": "string", "description": "The plan as markdown for the user to review." },
                    "steps": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "description": { "type": "string" },
                                "active_form": { "type": "string" }
                            },
                            "required": ["id", "description"]
                        }
                    }
                },
                "required": ["plan", "steps"]
            }),
        },
    ]
}

/// Apply a control-tool call to the plan state, returning the tool output the
/// model should see. `plan` is `&mut Option<PlanState>` on LoopState — created
/// lazily on first task creation.
///
/// `exit_plan_mode` returns a marker output; the loop inspects the tool name
/// to perform the actual plan-mode exit + accept/reject (Phase 3 wiring).
pub fn apply_control_tool(
    plan: &mut Option<PlanState>,
    tool_name: &str,
    args: &serde_json::Value,
) -> ToolOutput {
    match tool_name {
        TOOL_TASK_CREATE => {
            let steps = parse_steps(args.get("steps"));
            if steps.is_empty() {
                return fail("task_create requires a non-empty `steps` array.");
            }
            let mut state = plan.take().unwrap_or_default();
            state.set_steps(steps.clone());
            let summary = summarize(&state);
            *plan = Some(state);
            ok(format!("Created task list ({} steps).\n{}", summary.len_count, summary.text))
        }
        TOOL_TASK_UPDATE => {
            let id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
            let status = args
                .get("status")
                .and_then(|v| v.as_str())
                .and_then(parse_status)
                .unwrap_or(PlanStepStatus::Pending);
            let active_form = args
                .get("active_form")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let state = match plan.as_mut() {
                Some(s) => s,
                None => return fail("No task list exists. Call task_create first."),
            };
            match state.set_status(id, status) {
                Some(_) => {
                    if let Some(af) = active_form {
                        state.set_active_form(id, Some(af));
                    }
                    let summary = summarize(state);
                    ok(format!("Updated step {} → {:?}.\n{}", id, status, summary.text))
                }
                None => fail(&format!("Step '{}' not found in the task list.", id)),
            }
        }
        TOOL_TASK_LIST => {
            let state = match plan.as_ref() {
                Some(s) => s,
                None => return ok("No task list exists yet.".to_string()),
            };
            ok(summarize(state).text)
        }
        TOOL_EXIT_PLAN_MODE => {
            // Phase 3 wires the actual exit; here we just acknowledge so the
            // loop's interceptor can detect the call.
            let plan_text = args
                .get("plan")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            ok(format!("Plan submitted (awaiting approval):\n{}", plan_text))
        }
        _ => fail(&format!("Unknown control tool: {}", tool_name)),
    }
}

fn parse_steps(steps: Option<&serde_json::Value>) -> Vec<PlanStep> {
    let arr = match steps.and_then(|v| v.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };
    arr.iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.to_string();
            let description = item.get("description")?.as_str()?.to_string();
            let active_form = item.get("active_form").and_then(|v| v.as_str()).map(|s| s.to_string());
            Some(PlanStep {
                id,
                description,
                coupled: false,
                depends_on: vec![],
                status: PlanStepStatus::Pending,
                active_form,
            })
        })
        .collect()
}

/// Parse a `steps` array from a control-tool args value (shared by
/// `task_create` and `exit_plan_mode`). Public so the loop can extract the
/// proposed steps for the accept/reject gate.
pub fn extract_steps(args: &serde_json::Value) -> Vec<PlanStep> {
    parse_steps(args.get("steps"))
}

fn parse_status(s: &str) -> Option<PlanStepStatus> {
    match s {
        "pending" => Some(PlanStepStatus::Pending),
        "in_progress" => Some(PlanStepStatus::InProgress),
        "completed" => Some(PlanStepStatus::Completed),
        "failed" => Some(PlanStepStatus::Failed),
        _ => None,
    }
}

struct PlanSummary {
    text: String,
    len_count: String,
}

fn summarize(state: &PlanState) -> PlanSummary {
    let mut lines = Vec::with_capacity(state.steps.len());
    for step in &state.steps {
        let af = step
            .active_form
            .as_deref()
            .map(|f| format!(" ({})", f))
            .unwrap_or_default();
        lines.push(format!("{} [{}] {}{}", step.status.icon(), step.id, step.description, af));
    }
    let len_count = format!(
        "{} steps (✓{} / ◐{} / ✗{})",
        state.steps.len(),
        state.count_by_status(PlanStepStatus::Completed),
        state.count_by_status(PlanStepStatus::InProgress),
        state.count_by_status(PlanStepStatus::Failed),
    );
    PlanSummary {
        text: format!("{}\n{}", len_count, lines.join("\n")),
        len_count,
    }
}

fn ok(content: String) -> ToolOutput {
    ToolOutput { success: true, content, error: None }
}

fn fail(msg: &str) -> ToolOutput {
    ToolOutput { success: false, content: String::new(), error: Some(msg.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_update() {
        let mut plan = None;
        let out = apply_control_tool(
            &mut plan,
            TOOL_TASK_CREATE,
            &serde_json::json!({
                "steps": [
                    {"id": "1", "description": "write code"},
                    {"id": "2", "description": "test it", "active_form": "Testing"}
                ]
            }),
        );
        assert!(out.success);
        let state = plan.as_ref().unwrap();
        assert_eq!(state.steps.len(), 2);
        assert_eq!(state.revision, 1);
        assert_eq!(state.steps[1].active_form.as_deref(), Some("Testing"));

        let out = apply_control_tool(
            &mut plan,
            TOOL_TASK_UPDATE,
            &serde_json::json!({"task_id": "1", "status": "in_progress"}),
        );
        assert!(out.success);
        assert_eq!(plan.as_ref().unwrap().steps[0].status, PlanStepStatus::InProgress);
        assert!(plan.as_ref().unwrap().revision >= 2);

        let out = apply_control_tool(
            &mut plan,
            TOOL_TASK_UPDATE,
            &serde_json::json!({"task_id": "1", "status": "completed"}),
        );
        assert!(out.success);
        assert!(!plan.as_ref().unwrap().all_done());

        let out = apply_control_tool(
            &mut plan,
            TOOL_TASK_UPDATE,
            &serde_json::json!({"task_id": "2", "status": "completed"}),
        );
        assert!(plan.as_ref().unwrap().all_done());
    }

    #[test]
    fn test_update_missing_step() {
        let mut plan = None;
        apply_control_tool(
            &mut plan,
            TOOL_TASK_CREATE,
            &serde_json::json!({"steps": [{"id": "1", "description": "x"}]}),
        );
        let out = apply_control_tool(
            &mut plan,
            TOOL_TASK_UPDATE,
            &serde_json::json!({"task_id": "99", "status": "completed"}),
        );
        assert!(!out.success);
    }

    #[test]
    fn test_list_empty() {
        let mut plan = None;
        let out = apply_control_tool(&mut plan, TOOL_TASK_LIST, &serde_json::json!({}));
        assert!(out.success);
        assert!(out.content.contains("No task list"));
    }

    #[test]
    fn test_control_tool_defs_present() {
        let defs = control_tool_definitions();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&TOOL_TASK_CREATE));
        assert!(names.contains(&TOOL_TASK_UPDATE));
        assert!(names.contains(&TOOL_TASK_LIST));
        assert!(names.contains(&TOOL_EXIT_PLAN_MODE));
    }
}
