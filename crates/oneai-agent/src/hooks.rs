//! Lifecycle Hooks registry — manages and executes hooks at specific points
//! in the agent loop's lifecycle.
//!
//! This is the core of the "生命周期安全" (Lifecycle Security) evolution:
//! from ApprovalGate's gate-based approval to event-driven hooks at every
//! lifecycle stage. Inspired by Claude Code's hooks system.
//!
//! HookRegistry organizes hooks by their HookPoint and executes them in
//! registration order when the AgentLoop reaches the corresponding lifecycle stage.
//!
//! Resolution rules:
//! - **PreToolUse**: If any hook returns Deny, the overall result is Deny.
//!   If any hook returns Modify, the last Modify's args replace the original.
//!   Allow hooks are informational and don't affect the decision.
//! - **PostToolUse/PreInfer/PostInfer**: All hooks run sequentially.
//!   Modify hooks can transform output/request/response. Deny is treated as
//!   "replace with error message" rather than "block entirely" for post-phase hooks.
//! - **PreCheckpoint/Notification/Stop**: Hooks are informational only.
//!   Results are logged but don't affect execution flow.

use std::collections::HashMap;
use std::sync::Arc;

use oneai_core::{HookPoint, HookResult, HookContext};
use oneai_core::traits::LifecycleHook;

// ─── HookRegistry ────────────────────────────────────────────────────────────

/// Registry of lifecycle hooks, organized by HookPoint.
///
/// Hooks are registered via `register()` and executed via `run_hooks()`.
/// The registry maintains hooks grouped by their HookPoint for efficient
/// lookup during the agent loop.
///
/// Usage:
/// ```ignore
/// let registry = HookRegistry::new();
/// registry.register(Arc::new(AuditHook));
/// registry.register(Arc::new(SafetyConstraintHook));
///
/// // In the agent loop, at PreToolUse:
/// let results = registry.run_hooks(HookPoint::PreToolUse, context);
/// let final_result = resolve_hook_results(results);
/// ```
pub struct HookRegistry {
    /// Hooks organized by their registered HookPoint(s).
    /// Each point has a list of hooks that run in registration order.
    hooks: HashMap<HookPoint, Vec<Arc<dyn LifecycleHook>>>,
}

impl HookRegistry {
    /// Create a new empty hook registry.
    pub fn new() -> Self {
        Self {
            hooks: HashMap::new(),
        }
    }

    /// Register a lifecycle hook.
    ///
    /// The hook is added to all of its declared `points()` in the registry.
    /// Hooks at the same point execute in registration order.
    pub fn register(&mut self, hook: Arc<dyn LifecycleHook>) {
        for point in hook.points() {
            self.hooks.entry(point).or_default().push(hook.clone());
        }
    }

    /// Remove all hooks registered at a specific point.
    pub fn clear_point(&mut self, point: HookPoint) {
        self.hooks.remove(&point);
    }

    /// Remove all hooks from the registry.
    pub fn clear_all(&mut self) {
        self.hooks.clear();
    }

    /// Get the number of hooks registered at a specific point.
    pub fn count_at(&self, point: &HookPoint) -> usize {
        self.hooks.get(point).map_or(0, |v| v.len())
    }

    /// Get the total number of hooks across all points.
    pub fn total_count(&self) -> usize {
        self.hooks.values().map(|v| v.len()).sum()
    }

    /// Run all hooks registered at a given point.
    ///
    /// Hooks execute in registration order. Each hook receives the context
    /// and returns a HookResult. The results are collected and returned.
    ///
    /// For context that may be modified by hooks (e.g., tool_args in PreToolUse),
    /// the caller should use `resolve_pre_tool_use_results()` or
    /// `resolve_results()` to determine the final action.
    pub async fn run_hooks(&self, point: HookPoint, context: HookContext) -> Vec<HookResult> {
        let hooks = self.hooks.get(&point).cloned().unwrap_or_default();
        let mut results = Vec::new();

        for hook in hooks {
            tracing::debug!(
                "Running lifecycle hook '{}' at point {:?}",
                hook.name(),
                point
            );
            let result = hook.run(context.clone()).await;
            tracing::debug!(
                "Hook '{}' at {:?} returned: {:?}",
                hook.name(),
                point,
                result
            );
            results.push(result);
        }

        results
    }

    /// Resolve hook results for PreToolUse — determine the final action.
    ///
    /// Resolution rules:
    /// - If any hook returns Deny, the overall result is Deny (with the first Deny's reason).
    /// - If any hook returns Modify, the last Modify's args replace the original args.
    /// - If all hooks return Allow, the overall result is Allow.
    ///
    /// This follows the "strictest wins" principle: a single Deny overrides
    /// all Allows, and the last Modify takes effect (allowing progressive
    /// constraint enforcement).
    pub fn resolve_pre_tool_use_results(
        results: &[HookResult],
        original_args: &serde_json::Value,
    ) -> ResolvedHookAction {
        let mut deny_reason = None;
        let mut modified_args = None;

        for result in results {
            match result {
                HookResult::Allow => {}
                HookResult::Deny { reason } => {
                    // First Deny wins — no need to check further
                    deny_reason = Some(reason.clone());
                    break;
                }
                HookResult::Modify { modified_args: args } => {
                    // Last Modify wins — progressive modification
                    modified_args = Some(args.clone());
                }
            }
        }

        if let Some(reason) = deny_reason {
            ResolvedHookAction::Deny { reason }
        } else if let Some(args) = modified_args {
            ResolvedHookAction::Modify { modified_args: args }
        } else {
            ResolvedHookAction::Allow { args: original_args.clone() }
        }
    }

    /// Resolve hook results for general lifecycle points.
    ///
    /// For post-phase hooks (PostToolUse, PostInfer), the resolution is:
    /// - Deny → replace the output/response with an error message
    /// - Modify → replace the output/response with modified content
    /// - Allow → pass through unchanged
    ///
    /// The returned value indicates whether the original data should
    /// be kept, replaced, or blocked.
    pub fn resolve_results(results: &[HookResult]) -> ResolvedHookAction {
        let mut deny_reason = None;
        let mut modified_args = None;

        for result in results {
            match result {
                HookResult::Allow => {}
                HookResult::Deny { reason } => {
                    deny_reason = Some(reason.clone());
                    break;
                }
                HookResult::Modify { modified_args: args } => {
                    modified_args = Some(args.clone());
                }
            }
        }

        if let Some(reason) = deny_reason {
            ResolvedHookAction::Deny { reason }
        } else if let Some(args) = modified_args {
            ResolvedHookAction::Modify { modified_args: args }
        } else {
            ResolvedHookAction::Allow { args: serde_json::json!({}) }
        }
    }
}

impl Default for HookRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ─── ResolvedHookAction ──────────────────────────────────────────────────────

/// The final action after resolving all hook results at a lifecycle point.
///
/// This is what the AgentLoop uses to determine how to proceed:
/// - Allow: continue with the original (or specified) args
/// - Deny: skip the action and inject the error message
/// - Modify: proceed with the modified args
#[derive(Debug, Clone)]
pub enum ResolvedHookAction {
    /// Allow the action with these arguments (may be original or modified).
    Allow { args: serde_json::Value },

    /// Deny (block) the action with this reason.
    Deny { reason: String },

    /// Allow the action but with modified arguments.
    Modify { modified_args: serde_json::Value },
}

// ─── Built-in hooks ───────────────────────────────────────────────────────────

/// Audit logging hook — records all PreToolUse and PostToolUse events.
///
/// This hook always returns Allow (it doesn't block or modify anything),
/// but it logs every tool call for compliance/audit purposes.
pub struct AuditLogHook {
    /// Audit log entries (thread-safe for concurrent access).
    log: Arc<tokio::sync::RwLock<Vec<AuditEntry>>>,
}

/// An audit log entry recording a tool call event.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    /// When the event occurred.
    pub timestamp: String,
    /// Which hook point.
    pub point: HookPoint,
    /// The tool name (if applicable).
    pub tool_name: Option<String>,
    /// The tool arguments (if applicable).
    pub tool_args: Option<serde_json::Value>,
    /// The tool output (if applicable).
    pub tool_output_summary: Option<String>,
    /// The iteration number.
    pub iteration: usize,
}

impl AuditLogHook {
    /// Create a new audit log hook.
    pub fn new() -> Self {
        Self {
            log: Arc::new(tokio::sync::RwLock::new(Vec::new())),
        }
    }

    /// Get all audit log entries.
    pub async fn get_log(&self) -> Vec<AuditEntry> {
        self.log.read().await.clone()
    }

    /// Get the number of audit entries.
    pub async fn count(&self) -> usize {
        self.log.read().await.len()
    }
}

impl Default for AuditLogHook {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl LifecycleHook for AuditLogHook {
    fn points(&self) -> Vec<HookPoint> {
        vec![HookPoint::PreToolUse, HookPoint::PostToolUse]
    }

    async fn run(&self, context: HookContext) -> HookResult {
        let entry = AuditEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            point: context.point.clone(),
            tool_name: context.tool_name.clone(),
            tool_args: context.tool_args.clone(),
            tool_output_summary: context.tool_output.as_ref()
                .map(|o| if o.success { o.content.clone() } else { o.error.clone().unwrap_or_default() }),
            iteration: context.iteration,
        };
        self.log.write().await.push(entry);
        HookResult::Allow
    }

    fn name(&self) -> &str {
        "audit_log"
    }
}

/// Safety constraint hook — denies specific tool patterns.
///
/// This hook blocks tool calls that match configured deny patterns,
/// similar to DomainPack's PermissionProfile but at the hook level.
/// Useful for CI/CD scenarios where specific commands must never execute.
pub struct SafetyConstraintHook {
    /// Tool names that are always denied.
    denied_tools: Vec<String>,
    /// Tool name + arg substring combinations that are denied.
    /// (tool_name, arg_substring) — if args contain the substring, the call is denied.
    denied_patterns: Vec<(String, String)>,
    /// Tool name + arg modification rules.
    modified_args: HashMap<String, serde_json::Value>, // tool_name → fixed args
}

impl SafetyConstraintHook {
    /// Create a hook that denies specific tool names.
    pub fn deny_tools(tool_names: Vec<String>) -> Self {
        Self {
            denied_tools: tool_names,
            denied_patterns: Vec::new(),
            modified_args: HashMap::new(),
        }
    }

    /// Create a hook that denies a tool + arg substring pattern.
    pub fn deny_pattern(tool_name: String, arg_substring: String) -> Self {
        Self {
            denied_tools: Vec::new(),
            denied_patterns: vec![(tool_name, arg_substring)],
            modified_args: HashMap::new(),
        }
    }

    /// Create a hook that modifies args for a specific tool.
    pub fn modify_args(tool_name: String, fixed_args: serde_json::Value) -> Self {
        Self {
            denied_tools: Vec::new(),
            denied_patterns: Vec::new(),
            modified_args: HashMap::from([(tool_name, fixed_args)]),
        }
    }
}

#[async_trait::async_trait]
impl LifecycleHook for SafetyConstraintHook {
    fn points(&self) -> Vec<HookPoint> {
        vec![HookPoint::PreToolUse]
    }

    async fn run(&self, context: HookContext) -> HookResult {
        let tool_name = context.tool_name.as_deref().unwrap_or("");

        // Check denied tools
        if self.denied_tools.iter().any(|t| t == tool_name) {
            return HookResult::Deny {
                reason: format!("Tool '{}' is denied by safety constraint hook", tool_name),
            };
        }

        // Check denied patterns (substring matching)
        for (denied_tool, arg_substring) in &self.denied_patterns {
            if denied_tool == tool_name {
                let args_str = context.tool_args.as_ref()
                    .map(|a| a.to_string())
                    .unwrap_or_default();
                if args_str.contains(arg_substring) {
                    return HookResult::Deny {
                        reason: format!(
                            "Tool '{}' with args containing '{}' is denied by safety constraint hook",
                            tool_name, arg_substring
                        ),
                    };
                }
            }
        }

        // Check arg modifications
        if let Some(fixed_args) = self.modified_args.get(tool_name) {
            return HookResult::Modify {
                modified_args: fixed_args.clone(),
            };
        }

        HookResult::Allow
    }

    fn name(&self) -> &str {
        "safety_constraint"
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A simple test hook that always allows.
    struct AlwaysAllowHook;

    #[async_trait::async_trait]
    impl LifecycleHook for AlwaysAllowHook {
        fn points(&self) -> Vec<HookPoint> {
            vec![HookPoint::PreToolUse]
        }
        async fn run(&self, _context: HookContext) -> HookResult {
            HookResult::Allow
        }
        fn name(&self) -> &str { "always_allow" }
    }

    /// A test hook that always denies.
    struct AlwaysDenyHook;

    #[async_trait::async_trait]
    impl LifecycleHook for AlwaysDenyHook {
        fn points(&self) -> Vec<HookPoint> {
            vec![HookPoint::PreToolUse]
        }
        async fn run(&self, _context: HookContext) -> HookResult {
            HookResult::Deny { reason: "test deny".to_string() }
        }
        fn name(&self) -> &str { "always_deny" }
    }

    /// A test hook that modifies args.
    struct ModifyArgsHook;

    #[async_trait::async_trait]
    impl LifecycleHook for ModifyArgsHook {
        fn points(&self) -> Vec<HookPoint> {
            vec![HookPoint::PreToolUse]
        }
        async fn run(&self, _context: HookContext) -> HookResult {
            HookResult::Modify { modified_args: serde_json::json!({"modified": true}) }
        }
        fn name(&self) -> &str { "modify_args" }
    }

    #[test]
    fn test_hook_registry_register() {
        let mut registry = HookRegistry::new();
        registry.register(Arc::new(AlwaysAllowHook));
        assert_eq!(registry.count_at(&HookPoint::PreToolUse), 1);
        assert_eq!(registry.total_count(), 1);
    }

    #[test]
    fn test_hook_registry_multiple_points() {
        let mut registry = HookRegistry::new();
        registry.register(Arc::new(AuditLogHook::new()));
        assert_eq!(registry.count_at(&HookPoint::PreToolUse), 1);
        assert_eq!(registry.count_at(&HookPoint::PostToolUse), 1);
        assert_eq!(registry.total_count(), 2);
    }

    #[test]
    fn test_resolve_pre_tool_use_all_allow() {
        let results = vec![HookResult::Allow, HookResult::Allow];
        let action = HookRegistry::resolve_pre_tool_use_results(
            &results,
            &serde_json::json!({"path": "/test"}),
        );
        assert!(matches!(action, ResolvedHookAction::Allow { .. }));
    }

    #[test]
    fn test_resolve_pre_tool_use_deny_overrides_allow() {
        let results = vec![
            HookResult::Allow,
            HookResult::Deny { reason: "blocked".to_string() },
        ];
        let action = HookRegistry::resolve_pre_tool_use_results(
            &results,
            &serde_json::json!({}),
        );
        assert!(matches!(action, ResolvedHookAction::Deny { reason: _ }));
    }

    #[test]
    fn test_resolve_pre_tool_use_modify_overrides_allow() {
        let results = vec![
            HookResult::Allow,
            HookResult::Modify { modified_args: serde_json::json!({"safe": true}) },
        ];
        let action = HookRegistry::resolve_pre_tool_use_results(
            &results,
            &serde_json::json!({"dangerous": true}),
        );
        match action {
            ResolvedHookAction::Modify { modified_args } => {
                assert_eq!(modified_args["safe"], true);
            }
            _ => panic!("Expected Modify action"),
        }
    }

    #[test]
    fn test_resolve_pre_tool_use_deny_overrides_modify() {
        let results = vec![
            HookResult::Modify { modified_args: serde_json::json!({"safe": true}) },
            HookResult::Deny { reason: "blocked".to_string() },
        ];
        let action = HookRegistry::resolve_pre_tool_use_results(
            &results,
            &serde_json::json!({}),
        );
        assert!(matches!(action, ResolvedHookAction::Deny { .. }));
    }

    #[test]
    fn test_safety_constraint_hook_deny_tool() {
        let hook = SafetyConstraintHook::deny_tools(vec!["shell".to_string()]);
        let context = HookContext {
            point: HookPoint::PreToolUse,
            tool_name: Some("shell".to_string()),
            tool_args: Some(serde_json::json!({"command": "ls"})),
            tool_output: None,
            inference_request: None,
            inference_response: None,
            iteration: 1,
            paradigm: "ReAct".to_string(),
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(hook.run(context));
        assert!(matches!(result, HookResult::Deny { .. }));
    }

    #[test]
    fn test_safety_constraint_hook_allow_safe_tool() {
        let hook = SafetyConstraintHook::deny_tools(vec!["shell".to_string()]);
        let context = HookContext {
            point: HookPoint::PreToolUse,
            tool_name: Some("read_file".to_string()),
            tool_args: Some(serde_json::json!({"path": "/test"})),
            tool_output: None,
            inference_request: None,
            inference_response: None,
            iteration: 1,
            paradigm: "ReAct".to_string(),
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(hook.run(context));
        assert!(matches!(result, HookResult::Allow));
    }
}
