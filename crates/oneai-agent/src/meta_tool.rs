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

/// Whether a tool name is a model-driven meta-tool that the loop intercepts.
///
/// Use this as a defensive guard in the tool-dispatch path so a future routing
/// change can never accidentally send `delegate`/`switch_paradigm` to the
/// `ToolExecutor` (which would emit a "tool not found" error). Today
/// `parse_decision` converts these to `AgentDecision` before dispatch, so this
/// predicate is a backstop, not the primary filter.
pub fn is_meta_tool(name: &str) -> bool {
    matches!(name, TOOL_DELEGATE | TOOL_SWITCH_PARADIGM)
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
            description: "Delegate a self-contained subtask to a specialized sub-agent that runs in \
                its own context window, then returns a summary. Call this when: the subtask has a \
                clear boundary, the main loop does not need the intermediate steps, and you want to \
                preserve the main context for the overall task. After calling, the main loop waits \
                for the sub-agent's summary before continuing — do not also call other tools in the \
                same turn.".into(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "The self-contained subtask to delegate. Include enough context for the sub-agent to act independently."
                    },
                    "agent_type": {
                        "type": "string",
                        "enum": ["Plan", "Explore", "Code", "Review"],
                        "description": "The specialized sub-agent kind. Plan=decompose, Explore=search/understand, Code=implement/modify, Review=audit."
                    },
                    "budget_tokens": {
                        "type": "integer",
                        "description": "Token budget cap for the sub-agent (default 5000).",
                        "default": 5000
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
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_meta_tool() {
        assert!(is_meta_tool(TOOL_DELEGATE));
        assert!(is_meta_tool(TOOL_SWITCH_PARADIGM));
        assert!(!is_meta_tool("read_file"));
        assert!(!is_meta_tool("delegate_other"));
        assert!(!is_meta_tool(""));
    }

    #[test]
    fn test_meta_tool_definitions_shape() {
        let defs = meta_tool_definitions();
        assert_eq!(defs.len(), 2);

        let delegate = defs.iter().find(|d| d.name == TOOL_DELEGATE).unwrap();
        let schema = &delegate.parameters_schema;
        assert_eq!(schema["type"], "object");
        assert_eq!(
            schema["properties"]["agent_type"]["enum"],
            serde_json::json!(["Plan", "Explore", "Code", "Review"])
        );
        assert_eq!(
            schema["required"],
            serde_json::json!(["task", "agent_type"])
        );
        assert_eq!(schema["properties"]["budget_tokens"]["default"], 5000);

        let switch = defs.iter().find(|d| d.name == TOOL_SWITCH_PARADIGM).unwrap();
        let schema = &switch.parameters_schema;
        assert_eq!(schema["type"], "object");
        assert_eq!(
            schema["properties"]["paradigm"]["enum"],
            serde_json::json!(["plan", "react", "reflect", "explore"])
        );
        assert_eq!(schema["required"], serde_json::json!(["paradigm"]));
    }
}
