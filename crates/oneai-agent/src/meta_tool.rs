//! Meta-tools — model-driven control commands intercepted by the AgentLoop.
//!
//! Like [`crate::plan_state`]'s control tools, `delegate` and `switch_paradigm`
//! are *not* registered in the tool registry and are never dispatched to the
//! `ToolExecutor`. Instead their
//! [`ToolDefinition`](oneai_core::ToolDefinition)s are injected into the
//! inference request so the model can call them, and
//! `AgentLoop::parse_decision` intercepts the resulting `ToolCall` at the `ContentBlock` layer — turning it into an
//! [`AgentDecision::Delegate`](crate::AgentDecision::Delegate) or
//! [`AgentDecision::SwitchParadigm`](crate::AgentDecision::SwitchParadigm)
//! before it ever reaches the `filtered_calls` dispatch.
//!
//! This module only owns the *definitions* the model sees. The interception
//! routing already lives in `parse_decision`
//! (`agent_loop.rs`) and in the graph-side
//! `AgentLoopGraphActionExecutor::parse_decision`.
//!
//! See the design doc at the repo root:
//! `模型驱动的 delegate : switch_paradigm 端到端打通方案.md`.

/// Tool name for delegating a subtask to a specialized sub-agent. Set
/// `background=true` for fire-and-auto-notify (Phase 2A); default
/// `background=false` is the blocking batch.
pub const TOOL_DELEGATE: &str = "delegate";
/// Tool name for switching the active paradigm (entering a fixed graph flow).
pub const TOOL_SWITCH_PARADIGM: &str = "switch_paradigm";
/// Tool name for re-binding the project context to a different project root.
pub const TOOL_SWITCH_PROJECT: &str = "switch_project";

/// Whether a tool name is a model-driven meta-tool that the loop intercepts.
///
/// Use this as a defensive guard in the tool-dispatch path so a future routing
/// change can never accidentally send `delegate`/`switch_paradigm`/
/// `switch_project` to the `ToolExecutor` (which would emit a "tool not found"
/// error). Today `parse_decision` converts these to `AgentDecision` before
/// dispatch, so this predicate is a backstop, not the primary filter.
pub fn is_meta_tool(name: &str) -> bool {
    matches!(
        name,
        TOOL_DELEGATE | TOOL_SWITCH_PARADIGM | TOOL_SWITCH_PROJECT
    )
}

/// Whether a meta-tool is **loop-only** — handled by the main `AgentLoop` and
/// not modeled by the StateGraph `GraphDecision` executor. The graph-side
/// tool-definition builder filters these out so a StateGraph `LlmInfer` node
/// never advertises a meta-tool it can't honor. (`switch_project` needs
/// `AgentDecision::SwitchProject` which `GraphDecision` lacks.) `delegate`
/// foreground mode IS modeled by the graph executor, so it is NOT loop-only;
/// the graph simply never offers `background=true` (see
/// [`meta_tool_definitions`] with `background_enabled=false`).
pub fn is_loop_only_meta_tool(name: &str) -> bool {
    matches!(name, TOOL_SWITCH_PROJECT)
}

/// JSON-schema tool definitions for the meta-tools, injected into the
/// inference request so the model can call them.
///
/// `background_enabled`: whether the `delegate` tool advertises its
/// `background` parameter (fire-and-auto-notify mode). Only `true` on the main
/// loop when an `AsyncTaskRunner` is configured; `false` on no-runner loop
/// builds and on the StateGraph executor (neither can honor background mode, so
/// the parameter is omitted entirely — Footprint-gate discipline: never
/// advertise a mode the host can't fulfill).
///
/// The `agent_type` enum mirrors [`crate::sub_agent::SubAgentKind::from_str`]
/// (variants `Plan`/`Explore`/`Code`/`Review`/`Custom`); `from_str` lowercases
/// its input, so the capitalized enum values here parse correctly. The
/// `paradigm` enum mirrors the match arms in
/// `AgentLoop::parse_decision` (`plan`/`react`/`reflect`/`explore`).
pub fn meta_tool_definitions(background_enabled: bool) -> Vec<oneai_core::ToolDefinition> {
    vec![
        oneai_core::ToolDefinition {
            name: TOOL_DELEGATE.into(),
            description: format!(
                "Delegate one or more self-contained subtasks to specialized sub-agents that each run \
                in their OWN fresh context window (they do NOT see your conversation, prior tool \
                outputs, or other sub-agents' work), then return a summary. Call this when: the \
                subtask has a clear boundary, the main loop does not need the intermediate steps, \
                and you want to preserve the main context for the overall task.\n\n\
                WHEN NOT TO delegate — do it directly in the main loop instead:\n\
                - Interdependent code-gen: if subtasks must read/modify each other's files (e.g. \
                'board module' and 'game loop' that imports it), parallel sub-agents CANNOT see \
                each other's work — they'll fail on missing files/exit-code errors and waste their \
                budget. Sequence interdependent work in the main loop, or fan out only TRULY \
                independent pieces.\n\
                - A single-file edit or a 2–3 file change — apply_patch directly is faster.\n\
                - A specific search — use grep/glob/read_file directly.\n\n\
                WHEN YOU DO delegate — the sub-agent starts blind, so the `task` string MUST be a \
                highly detailed, self-contained spec: the exact files/paths/conventions to use, \
                precise requirements, any interfaces it must match, AND how to verify its own work \
                (e.g. the test/lint command to run). Specify exactly what it should return in its \
                summary. A vague one-liner ('build the board') makes the sub-agent flail and exhaust \
                its budget — a detailed spec makes it succeed in a few iterations.\n\n\
                You MAY call `delegate` multiple times in the same turn to fan out several \
                independent subtasks (they run in parallel). Do not also call non-delegate tools in \
                the same turn. `depends_on` only sequences foreground (blocking) mode.\n\n\
                Specialization (role layering, all optional): set `agent_type`=\"Custom\" with \
                `custom_role` to mint a specialized sub-agent that the fixed kinds don't cover; pass \
                `system_prompt` to override the kind's default role prompt; pass `tools` to NARROW the \
                sub-agent's toolset below the kind default (never widen — out-of-set names are dropped). \
                Set `inherit_context`=true to seed the sub-agent with your last `inherit_last_n_messages` \
                turns (defaults to 6 when true and 0) so it starts from your current reasoning instead \
                of from scratch — use this for \"continue from where I am\" subtasks.{}",
                if background_enabled {
                    "\n\n\
                    BACKGROUND MODE (set `background`=true): launch the sub-agent detached and return \
                    immediately — you do NOT wait, do NOT poll, and do NOT re-issue a task already in \
                    flight. You will be notified automatically when it finishes (a new message with its \
                    result arrives and re-activates you). Use this instead of foreground only when the \
                    subtask is long-running AND independent.\n\n\
                    After launching background tasks, do ONE of:\n\
                    - continue with DIFFERENT non-overlapping work (your own tool calls), OR\n\
                    - END YOUR RESPONSE and wait for the completion notifications to resume you.\n\
                    Do NOT keep producing pure reasoning turns while background sub-agents run — that \
                    competes with them for the same provider and starves everyone. If you have nothing \
                    else to do, end your response. Sequence dependent work across turns by waiting for \
                    each task's notification, NOT via `depends_on` (it is ignored in background mode).\n\n\
                    A `[Background tasks] (live)` block is injected into your context each step listing \
                    every in-flight background task and its status (Running/Completed/Failed). READ it \
                    before delegating: never re-delegate a task that is already `Running` or `Completed` \
                    — that is wasted duplicate work."
                } else {
                    ""
                }
            ),
            parameters_schema: {
                let mut props = serde_json::json!({
                    "task": {
                        "type": "string",
                        "description": "The self-contained subtask to delegate. Include enough context for the sub-agent to act independently."
                    },
                    "agent_type": {
                        "type": "string",
                        "enum": ["Plan", "Explore", "Code", "Review", "Custom"],
                        "description": "The specialized sub-agent kind. Plan=decompose, Explore=search/understand, Code=implement/modify, Review=audit, Custom=a specialized role you name via `custom_role`."
                    },
                    "budget_tokens": {
                        "type": "integer",
                        "description": "Token budget cap for the sub-agent (default 5000). For code-generation tasks, set this high enough (e.g. 20000+) for the sub-agent to actually finish.",
                        "default": 5000
                    },
                    "id": {
                        "type": "string",
                        "description": "Stable identifier for this delegation, so other delegations in the same turn can reference it via `depends_on` (foreground only). If omitted, one is assigned automatically."
                    },
                    "depends_on": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Ids of foreground delegations in the same turn that must complete before this one starts. Their summaries are automatically prepended to this subtask. Ignored in background mode. Omit for parallel/independent subtasks."
                    },
                    "custom_role": {
                        "type": "string",
                        "description": "Used only when `agent_type`=\"Custom\". Names the custom role (e.g. \"security-reviewer\", \"test-writer\"). Ignored for the fixed kinds."
                    },
                    "system_prompt": {
                        "type": "string",
                        "description": "Override the kind's default system prompt for this delegation. Use to specialize a role (e.g. a reviewer focused only on concurrency bugs). Optional."
                    },
                    "tools": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Narrow the sub-agent's toolset to this subset of the kind's default tools. Names outside the default set are dropped (never widened). Optional."
                    },
                    "inherit_context": {
                        "type": "boolean",
                        "description": "If true, seed the sub-agent with the parent loop's recent turns so it continues from the parent's current reasoning (Fork-style). Default false.",
                        "default": false
                    },
                    "inherit_last_n_messages": {
                        "type": "integer",
                        "description": "When `inherit_context` is true, how many of the parent's trailing non-system messages to seed. 0 with inherit_context=true defaults to 6.",
                        "default": 0
                    }
                });
                if background_enabled {
                    props["background"] = serde_json::json!({
                        "type": "boolean",
                        "description": "If true, run the sub-agent in the BACKGROUND (fire-and-auto-notify): the call returns immediately, the sub-agent runs detached, and you are notified automatically when it finishes. DO NOT poll, call task_status, or re-issue a background task already running. If false (default), the sub-agent runs in the foreground and you await its summary before continuing.",
                        "default": false
                    });
                }
                serde_json::json!({
                    "type": "object",
                    "properties": props,
                    "required": ["task", "agent_type"]
                })
            },
        },
        oneai_core::ToolDefinition {
            name: TOOL_SWITCH_PARADIGM.into(),
            description: "Switch the active paradigm, entering the corresponding fixed graph flow. \
                Call this when the ReAct (reason-then-act) loop is not the right shape for the \
                current subtask: use \"plan\" for structured decomposition, \"reflect\" for deep \
                review of the last result, \"explore\" for breadth-first search, or \"react\" to \
                return to the standard loop. After calling, execution continues inside the target \
                paradigm's graph and the result is fed back to the main loop.".into(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "paradigm": {
                        "type": "string",
                        "enum": ["plan", "react", "reflect", "explore"],
                        "description": "The target paradigm to switch into."
                    }
                },
                "required": ["paradigm"]
            }),
        },
        oneai_core::ToolDefinition {
            name: TOOL_SWITCH_PROJECT.into(),
            description: "Re-bind the project *context* (project instructions, repo map, \
                file tree, project config, git status) to a different project root directory. \
                Call this FIRST, before any other work, when the task concerns a project that \
                lives at a different path than the currently-injected project context — so you \
                operate on accurate (non-redundant) information instead of carrying the wrong \
                project's CLAUDE.md / repo map / file tree / config / git status. Pass the \
                absolute path to the target project's root directory. The new context is \
                injected on the next iteration. Note: this re-binds context only; the file-tool \
                and shell sandboxes stay scoped to the startup project, so use absolute paths \
                via shell for file operations on the new project.".into(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "project_dir": {
                        "type": "string",
                        "description": "Absolute path to the target project's root directory."
                    }
                },
                "required": ["project_dir"]
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_meta_tool() {
        assert!(is_meta_tool(TOOL_DELEGATE));
        assert!(is_meta_tool(TOOL_SWITCH_PARADIGM));
        assert!(is_meta_tool(TOOL_SWITCH_PROJECT));
        assert!(!is_meta_tool("read_file"));
        assert!(!is_meta_tool("delegate_background")); // unified into `delegate`
        assert!(!is_meta_tool("delegate_other"));
        assert!(!is_meta_tool(""));
    }

    #[test]
    fn test_is_loop_only_meta_tool() {
        // Only `switch_project` is loop-only (SwitchProject needs
        // AgentDecision::SwitchProject which GraphDecision lacks). `delegate`
        // foreground mode IS modeled by the graph executor (background mode is
        // simply not advertised to the graph — meta_tool_definitions(false)).
        assert!(is_loop_only_meta_tool(TOOL_SWITCH_PROJECT));
        assert!(!is_loop_only_meta_tool(TOOL_DELEGATE));
        assert!(!is_loop_only_meta_tool(TOOL_SWITCH_PARADIGM));
    }

    #[test]
    fn test_meta_tool_definitions_shape() {
        // With background mode advertised (main loop + runner).
        let defs = meta_tool_definitions(true);
        assert_eq!(defs.len(), 3);
        let delegate = defs.iter().find(|d| d.name == TOOL_DELEGATE).unwrap();
        let schema = &delegate.parameters_schema;
        assert_eq!(schema["type"], "object");
        assert_eq!(
            schema["properties"]["agent_type"]["enum"],
            serde_json::json!(["Plan", "Explore", "Code", "Review", "Custom"])
        );
        assert_eq!(
            schema["required"],
            serde_json::json!(["task", "agent_type"])
        );
        assert_eq!(schema["properties"]["budget_tokens"]["default"], 5000);
        assert_eq!(schema["properties"]["id"]["type"], "string");
        assert_eq!(schema["properties"]["depends_on"]["type"], "array");
        assert_eq!(
            schema["properties"]["depends_on"]["items"]["type"],
            "string"
        );
        assert_eq!(schema["properties"]["custom_role"]["type"], "string");
        assert_eq!(schema["properties"]["system_prompt"]["type"], "string");
        assert_eq!(schema["properties"]["tools"]["type"], "array");
        assert_eq!(schema["properties"]["inherit_context"]["type"], "boolean");
        assert_eq!(
            schema["properties"]["inherit_last_n_messages"]["type"],
            "integer"
        );
        // Background mode advertised.
        assert_eq!(schema["properties"]["background"]["type"], "boolean");
        assert_eq!(schema["properties"]["background"]["default"], false);

        // Without background mode (no-runner loop build / graph executor) — the
        // `background` field is omitted entirely (Footprint-gate: don't
        // advertise a mode the host can't fulfill).
        let defs_no_bg = meta_tool_definitions(false);
        let delegate = defs_no_bg.iter().find(|d| d.name == TOOL_DELEGATE).unwrap();
        assert!(
            delegate.parameters_schema["properties"]
                .get("background")
                .is_none(),
            "background field must be omitted when background_enabled=false"
        );

        // No separate delegate_background / polling / collect tools — unified.
        for defs in [defs.as_slice(), defs_no_bg.as_slice()] {
            assert!(defs.iter().all(|d| d.name != "delegate_background"));
            assert!(defs.iter().all(|d| d.name != "task_status"));
            assert!(defs.iter().all(|d| d.name != "collect_results"));
        }
    }
}
