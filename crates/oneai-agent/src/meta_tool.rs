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

/// Tool name for delegating a subtask to a specialized sub-agent.
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

/// JSON-schema tool definitions for the two meta-tools, injected into the
/// inference request so the model can call them.
///
/// The `agent_type` enum mirrors [`crate::sub_agent::SubAgentKind::from_str`]
/// (variants `Plan`/`Explore`/`Code`/`Review`/`Custom`); `from_str` lowercases
/// its input, so the capitalized enum values here parse correctly. The
/// `paradigm` enum mirrors the match arms in
/// `AgentLoop::parse_decision` (`plan`/`react`/`reflect`/`explore`).
pub fn meta_tool_definitions() -> Vec<oneai_core::ToolDefinition> {
    vec![
        oneai_core::ToolDefinition {
            name: TOOL_DELEGATE.into(),
            description: "Delegate one or more self-contained subtasks to specialized sub-agents that each run \
                in their own context window, then return a summary. Call this when: the subtask has a \
                clear boundary, the main loop does not need the intermediate steps, and you want to \
                preserve the main context for the overall task.\n\n\
                You MAY call `delegate` multiple times in the same turn to fan out several subtasks. \
                Subtasks with no `depends_on` run in parallel; a subtask that lists `depends_on` ids \
                runs only after those subtasks finish, and its task description is automatically \
                prefixed with their summaries — so a dependent subtask receives its upstream results \
                without you re-stating them. Do not also call non-delegate tools in the same turn.\n\n\
                Specialization (role layering, all optional): set `agent_type`=\"Custom\" with \
                `custom_role` to mint a specialized sub-agent that the fixed kinds don't cover; pass \
                `system_prompt` to override the kind's default role prompt; pass `tools` to NARROW the \
                sub-agent's toolset below the kind default (never widen — out-of-set names are dropped). \
                Set `inherit_context`=true to seed the sub-agent with your last `inherit_last_n_messages` \
                turns (defaults to 6 when true and 0) so it starts from your current reasoning instead \
                of from scratch — use this only for \"continue from where I am\" subtasks.".into(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
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
                        "description": "Token budget cap for the sub-agent (default 5000).",
                        "default": 5000
                    },
                    "id": {
                        "type": "string",
                        "description": "Stable identifier for this delegation, so other delegations in the same turn can reference it via `depends_on`. If omitted, one is assigned automatically; supplying it is recommended whenever you set `depends_on` on any delegation."
                    },
                    "depends_on": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Ids of delegations in the same turn that must complete before this one starts. Their summaries are automatically prepended to this subtask. Omit (or leave empty) for subtasks that can run in parallel."
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
                },
                "required": ["task", "agent_type"]
            }),
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
        assert!(!is_meta_tool("delegate_other"));
        assert!(!is_meta_tool(""));
    }

    #[test]
    fn test_meta_tool_definitions_shape() {
        let defs = meta_tool_definitions();
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
        // New dependency fields allow multi-delegate fan-out per turn.
        assert_eq!(schema["properties"]["id"]["type"], "string");
        assert_eq!(schema["properties"]["depends_on"]["type"], "array");
        assert_eq!(
            schema["properties"]["depends_on"]["items"]["type"],
            "string"
        );
        // Opt 3 specialization fields.
        assert_eq!(schema["properties"]["custom_role"]["type"], "string");
        assert_eq!(schema["properties"]["system_prompt"]["type"], "string");
        assert_eq!(schema["properties"]["tools"]["type"], "array");
        assert_eq!(schema["properties"]["tools"]["items"]["type"], "string");
        // Opt 4 context-inheritance fields.
        assert_eq!(schema["properties"]["inherit_context"]["type"], "boolean");
        assert_eq!(
            schema["properties"]["inherit_last_n_messages"]["type"],
            "integer"
        );

        let switch = defs
            .iter()
            .find(|d| d.name == TOOL_SWITCH_PARADIGM)
            .unwrap();
        let schema = &switch.parameters_schema;
        assert_eq!(schema["type"], "object");
        assert_eq!(
            schema["properties"]["paradigm"]["enum"],
            serde_json::json!(["plan", "react", "reflect", "explore"])
        );
        assert_eq!(schema["required"], serde_json::json!(["paradigm"]));

        let switch_proj = defs.iter().find(|d| d.name == TOOL_SWITCH_PROJECT).unwrap();
        let schema = &switch_proj.parameters_schema;
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["project_dir"]["type"], "string");
        assert_eq!(schema["required"], serde_json::json!(["project_dir"]));
    }
}
