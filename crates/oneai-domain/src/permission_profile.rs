//! PermissionProfile — domain-specific permission classification.
//!
//! The PermissionProfile provides a domain-level override layer for tool
//! permission decisions. The current system determines tool permission by
//! the individual tool's `risk_level()` method. PermissionProfile adds
//! domain-level rules that override or supplement the tool-level defaults.
//!
//! Resolution order (most authoritative first):
//! 1. `deny_by_default` — always deny if a tool+args pattern matches
//! 2. `permission_overrides` — override the tool's default PermissionLevel
//! 3. `auto_approve` — skip the approval gate entirely for these tools
//! 4. `require_confirmation` — always route through the approval gate
//! 5. Fall back to the tool's own `risk_level()` converted to PermissionLevel
//!
//! When multiple DomainPacks are combined, their PermissionProfiles are
//! merged using the "strictest wins" rule.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use oneai_core::{ApprovalPolicy, PermissionLevel};
use serde::{Deserialize, Serialize};

// ─── DenyPattern ───────────────────────────────────────────────────────────────

/// A pattern that causes a tool or tool+args combination to be always denied.
///
/// Deny patterns are the highest-priority permission rule — they override
/// everything else. If a tool call matches a deny pattern, it is blocked
/// regardless of any other approval configuration.
///
/// Examples:
/// - Block `shell(rm -rf /)` — irreversible root deletion
/// - Block `shell(format*)` — filesystem formatting
/// - Block `shell(drop*)` — database table deletion
/// - Block `send_command(factory_reset)` — IoT factory reset
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DenyPattern {
    /// Tool name pattern. Supports exact match ("shell") or regex ("shell|execute").
    pub tool_pattern: String,

    /// Optional regex pattern matching tool arguments.
    /// When present, denial only triggers when both tool_pattern AND arg_pattern match.
    /// Example: shell tool with arg_pattern "rm.*-rf" blocks `rm -rf` but not `ls`.
    pub arg_pattern: Option<String>,

    /// Reason for denial — shown to the user and model as explanation.
    pub reason: String,
}

impl DenyPattern {
    /// Create a simple deny pattern that blocks a tool entirely.
    pub fn deny_tool(tool_name: &str, reason: &str) -> Self {
        Self {
            tool_pattern: tool_name.to_string(),
            arg_pattern: None,
            reason: reason.to_string(),
        }
    }

    /// Create a deny pattern that blocks specific tool arguments.
    pub fn deny_tool_args(tool_name: &str, arg_pattern: &str, reason: &str) -> Self {
        Self {
            tool_pattern: tool_name.to_string(),
            arg_pattern: Some(arg_pattern.to_string()),
            reason: reason.to_string(),
        }
    }

    /// Check if a tool call matches this deny pattern.
    ///
    /// Returns true if the tool name matches `tool_pattern` and
    /// (if arg_pattern is present) the serialized args string matches it.
    pub fn matches(&self, tool_name: &str, args: &serde_json::Value) -> bool {
        // Tool name match: exact match or regex
        let tool_matches = self.tool_pattern == tool_name
            || regex::Regex::new(&self.tool_pattern)
                .map(|re| re.is_match(tool_name))
                .unwrap_or(false);

        if !tool_matches {
            return false;
        }

        // Arg pattern match (if present)
        if let Some(arg_pattern) = &self.arg_pattern {
            let args_str = args.to_string();
            regex::Regex::new(arg_pattern)
                .map(|re| re.is_match(&args_str))
                .unwrap_or(false)
        } else {
            true // No arg pattern → match on tool name alone
        }
    }
}

// ─── PermissionAction ──────────────────────────────────────────────────────────

// `PermissionAction` now lives in `oneai-core` (see `oneai_core::PermissionAction`)
// so that `oneai-tool` / `oneai-workflow` can honour domain permission policy via
// the `PermissionResolver` trait without depending on `oneai-domain` (the
// dependency direction is `oneai-domain → oneai-tool`). Re-exported here for
// backward-compatible `oneai_domain::PermissionAction` access.
pub use oneai_core::PermissionAction;

// ─── PermissionProfile ─────────────────────────────────────────────────────────

/// Domain-specific permission classification for tool execution.
///
/// PermissionProfile provides a domain-level override layer that determines
/// how tool calls are approved or denied. This is the "3rd layer" of the
/// DomainPack system: domain-specific permission rules.
///
/// Examples:
/// - CodingPack: auto-approve read/grep/glob, confirm edit/shell, deny shell(rm*)
/// - ResearchPack: auto-approve web_search/pdf_read, confirm web_fetch, deny shell
/// - DataAnalysisPack: auto-approve query_db, confirm data_transform, deny drop*
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PermissionProfile {
    /// Human-readable name for this profile (e.g., "coding", "research").
    pub name: String,

    /// Tools that are auto-approved in this domain.
    /// These tools bypass the approval gate entirely — their calls execute directly.
    /// Use for read-only/observation tools that never modify state.
    pub auto_approve: HashSet<String>,

    /// Tools that require human confirmation in this domain.
    /// These tools always route through the approval gate, regardless of their
    /// inherent risk level. Use for state-modifying tools that should be
    /// supervised in this domain context.
    pub require_confirmation: HashSet<String>,

    /// Patterns that cause tool calls to be always denied.
    /// Deny patterns have the highest priority — they override everything.
    pub deny_by_default: Vec<DenyPattern>,

    /// Per-tool PermissionLevel overrides.
    /// When a tool name has an explicit override, it replaces the tool's
    /// default `risk_level()` conversion. Use for domain-specific risk
    /// reclassification (e.g., shell is Full in coding but denied in research).
    pub permission_overrides: HashMap<String, PermissionLevel>,

    /// The default approval threshold for this domain.
    /// When no profile-specific rule exists, this threshold determines
    /// which PermissionLevels require approval.
    pub default_threshold: PermissionLevel,

    /// The Guardian's `AskForApproval` policy (#28 Stage 2) — when to prompt
    /// the user given a Guardian verdict on a tool call's content. Defaults to
    /// `OnFailure` (auto-decide safe/destructive, prompt only when uncertain).
    /// `Never` for headless/CI; `OnRequest` for conservative domains. The
    /// AppBuilder feeds this into the `GuardianContext`.
    #[serde(default)]
    pub approval_policy: ApprovalPolicy,

    /// Directories the Guardian treats as "trusted" for
    /// `OnUntrustedDir` (and as the cwd-trust baseline). Empty → trust the
    /// working dir itself. The AppBuilder fills the project root here.
    #[serde(default)]
    pub trusted_dirs: Vec<PathBuf>,

    /// #28 Stage 4 — config-driven token-prefix rule layer (`ExecPolicy`). A
    /// matching rule emits a [`Verdict`](oneai_core::Verdict) directly, skipping
    /// the Guardian reviewer heuristic. `None` / empty → the reviewer decides
    /// (pre-Stage-4 behaviour). Default `None`; a DomainPack configures rules.
    #[serde(default)]
    pub exec_policy: Option<oneai_tool::ExecPolicy>,
}

impl PermissionProfile {
    /// Create an empty permission profile with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            auto_approve: HashSet::new(),
            require_confirmation: HashSet::new(),
            deny_by_default: Vec::new(),
            permission_overrides: HashMap::new(),
            default_threshold: PermissionLevel::Standard,
            approval_policy: ApprovalPolicy::OnFailure,
            trusted_dirs: Vec::new(),
            exec_policy: None,
        }
    }

    /// Resolve the permission action for a tool call.
    ///
    /// Applies the 5-step resolution chain:
    /// 1. Check deny_by_default → Deny
    /// 2. Check auto_approve → AutoApprove
    /// 3. Check require_confirmation → RequireConfirmation
    /// 4. Check permission_overrides → Use overridden level
    /// 5. Fall back → UseDefaultPermission
    pub fn resolve(&self, tool_name: &str, args: &serde_json::Value) -> PermissionAction {
        // Step 1: Check deny patterns (highest priority)
        for pattern in &self.deny_by_default {
            if pattern.matches(tool_name, args) {
                return PermissionAction::Deny {
                    reason: pattern.reason.clone(),
                };
            }
        }

        // Step 2: Check auto_approve
        if self.auto_approve.contains(tool_name) {
            return PermissionAction::AutoApprove;
        }

        // Step 3: Check require_confirmation
        if self.require_confirmation.contains(tool_name) {
            return PermissionAction::RequireConfirmation;
        }

        // Step 4: Check permission overrides
        if let Some(level) = self.permission_overrides.get(tool_name) {
            return PermissionAction::UseDefaultPermission { level: *level };
        }

        // Step 5: No domain rule — fall back to tool's default
        PermissionAction::UseDefaultPermission {
            level: self.default_threshold,
        }
    }

    /// Merge two PermissionProfiles using the "strictest wins" rule.
    ///
    /// - auto_approve: intersection only (a tool must be in BOTH packs' auto_approve)
    /// - require_confirmation: union (a tool in ANY pack's require_confirmation goes there)
    /// - deny_by_default: union (all deny patterns from both packs)
    /// - permission_overrides: take the stricter level (Read < Standard < Full)
    /// - default_threshold: take the stricter level
    pub fn merge_strictest(a: &Self, b: &Self) -> Self {
        let name = format!("{}_{}_merged", a.name, b.name);

        // Auto-approve: intersection (must be approved in BOTH domains)
        let auto_approve = a
            .auto_approve
            .intersection(&b.auto_approve)
            .cloned()
            .collect();

        // Require confirmation: union (confirmed in ANY domain)
        let require_confirmation = a
            .require_confirmation
            .union(&b.require_confirmation)
            .cloned()
            .collect();

        // Deny patterns: union
        let deny_by_default = a
            .deny_by_default
            .iter()
            .cloned()
            .chain(b.deny_by_default.iter().cloned())
            .collect();

        // Permission overrides: take stricter level
        let mut permission_overrides = a.permission_overrides.clone();
        for (tool, level_b) in &b.permission_overrides {
            match permission_overrides.get(tool) {
                Some(level_a) => {
                    // Take the stricter one
                    permission_overrides.insert(tool.clone(), stricter_level(*level_a, *level_b));
                }
                None => {
                    permission_overrides.insert(tool.clone(), *level_b);
                }
            }
        }

        // Default threshold: take stricter
        let default_threshold = stricter_level(a.default_threshold, b.default_threshold);

        // #28 Stage 2 — approval_policy + trusted_dirs merge. The strictest
        // policy is the one that asks the most (OnRequest > OnUntrustedDir >
        // OnFailure > Never): a multi-domain agent inherits the more-
        // conservative domain's prompting posture. trusted_dirs is the
        // intersection (a dir must be trusted by BOTH to stay trusted).
        let approval_policy = stricter_policy(a.approval_policy, b.approval_policy);
        let trusted_dirs = a
            .trusted_dirs
            .iter()
            .filter(|d| b.trusted_dirs.contains(d))
            .cloned()
            .collect();

        // #28 Stage 4 — exec_policy merge: union both domains' kept rules
        // (strictest-wins per-evaluation means a Deny in either domain still
        // denies). `None` from either side yields the other's policy; both
        // `None` → `None`.
        let exec_policy = match (a.exec_policy.as_ref(), b.exec_policy.as_ref()) {
            (None, None) => None,
            (Some(x), None) | (None, Some(x)) => Some(x.clone()),
            (Some(a_ep), Some(b_ep)) => {
                let mut rules = a_ep.rules().to_vec();
                rules.extend(b_ep.rules().iter().cloned());
                Some(oneai_tool::ExecPolicy::from_rules(rules))
            }
        };

        Self {
            name,
            auto_approve,
            require_confirmation,
            deny_by_default,
            permission_overrides,
            default_threshold,
            approval_policy,
            trusted_dirs,
            exec_policy,
        }
    }
}

impl Default for PermissionProfile {
    fn default() -> Self {
        Self::new("default")
    }
}

impl oneai_core::PermissionResolver for PermissionProfile {
    /// Delegate to the inherent `resolve` — makes a `PermissionProfile`
    /// injectable into `ToolExecutor` / `WorkflowExecutor` (which live below
    /// `oneai-domain` in the dependency graph and so can only see the
    /// `PermissionResolver` trait, not this concrete type).
    fn resolve(&self, tool_name: &str, args: &serde_json::Value) -> PermissionAction {
        PermissionProfile::resolve(self, tool_name, args)
    }
}

/// Return the stricter of two PermissionLevels.
///
/// Ordering: Read < Standard < Full (Read is safest, Full is most dangerous).
/// "Stricter" means the level that requires more approval:
/// - Between Read and Standard, Standard is stricter (requires more approval)
/// - Between Standard and Full, Full is stricter
fn stricter_level(a: PermissionLevel, b: PermissionLevel) -> PermissionLevel {
    match (a, b) {
        (PermissionLevel::Read, PermissionLevel::Read) => PermissionLevel::Read,
        (PermissionLevel::Read, PermissionLevel::Standard) => PermissionLevel::Standard,
        (PermissionLevel::Read, PermissionLevel::Full) => PermissionLevel::Full,
        (PermissionLevel::Standard, PermissionLevel::Read) => PermissionLevel::Standard,
        (PermissionLevel::Standard, PermissionLevel::Standard) => PermissionLevel::Standard,
        (PermissionLevel::Standard, PermissionLevel::Full) => PermissionLevel::Full,
        (PermissionLevel::Full, PermissionLevel::Read) => PermissionLevel::Full,
        (PermissionLevel::Full, PermissionLevel::Standard) => PermissionLevel::Full,
        (PermissionLevel::Full, PermissionLevel::Full) => PermissionLevel::Full,
    }
}

/// The stricter (more-asking) of two `ApprovalPolicy`s. Strictness order:
/// `OnRequest` > `OnUntrustedDir` > `OnFailure` > `Never`. A multi-domain agent
/// inherits the more conservative domain's prompting posture.
fn stricter_policy(a: ApprovalPolicy, b: ApprovalPolicy) -> ApprovalPolicy {
    fn rank(p: ApprovalPolicy) -> u8 {
        match p {
            ApprovalPolicy::OnRequest => 3,
            ApprovalPolicy::OnUntrustedDir => 2,
            ApprovalPolicy::OnFailure => 1,
            ApprovalPolicy::Never => 0,
            _ => 3, // unknown variants default to strictest
        }
    }
    if rank(b) > rank(a) {
        b
    } else {
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deny_pattern_exact_match() {
        let pattern = DenyPattern::deny_tool("shell", "Dangerous");
        assert!(pattern.matches("shell", &serde_json::json!({})));
        assert!(!pattern.matches("read_file", &serde_json::json!({})));
    }

    #[test]
    fn test_deny_pattern_with_args() {
        let pattern = DenyPattern::deny_tool_args("shell", "rm.*-rf", "Irreversible deletion");
        assert!(pattern.matches("shell", &serde_json::json!({"command": "rm -rf /"})));
        assert!(!pattern.matches("shell", &serde_json::json!({"command": "ls"})));
    }

    #[test]
    fn test_permission_profile_resolve_deny() {
        let profile = PermissionProfile {
            name: "coding".to_string(),
            auto_approve: HashSet::new(),
            require_confirmation: HashSet::new(),
            deny_by_default: vec![DenyPattern::deny_tool_args(
                "shell",
                "rm.*-rf",
                "Root deletion",
            )],
            permission_overrides: HashMap::new(),
            default_threshold: PermissionLevel::Standard,
            ..Default::default()
        };

        let action = profile.resolve("shell", &serde_json::json!({"command": "rm -rf /"}));
        assert_eq!(
            action,
            PermissionAction::Deny {
                reason: "Root deletion".to_string()
            }
        );
    }

    #[test]
    fn test_permission_profile_resolve_auto_approve() {
        let profile = PermissionProfile {
            name: "coding".to_string(),
            auto_approve: HashSet::from(["read_file".to_string(), "grep".to_string()]),
            require_confirmation: HashSet::new(),
            deny_by_default: Vec::new(),
            permission_overrides: HashMap::new(),
            default_threshold: PermissionLevel::Standard,
            ..Default::default()
        };

        let action = profile.resolve("read_file", &serde_json::json!({"path": "/tmp/test"}));
        assert_eq!(action, PermissionAction::AutoApprove);
    }

    #[test]
    fn test_permission_profile_resolve_require_confirmation() {
        let profile = PermissionProfile {
            name: "coding".to_string(),
            auto_approve: HashSet::from(["read_file".to_string()]),
            require_confirmation: HashSet::from(["shell".to_string()]),
            deny_by_default: Vec::new(),
            permission_overrides: HashMap::new(),
            default_threshold: PermissionLevel::Standard,
            ..Default::default()
        };

        let action = profile.resolve("shell", &serde_json::json!({"command": "echo hi"}));
        assert_eq!(action, PermissionAction::RequireConfirmation);
    }

    #[test]
    fn test_permission_profile_resolve_override() {
        let profile = PermissionProfile {
            name: "research".to_string(),
            auto_approve: HashSet::new(),
            require_confirmation: HashSet::new(),
            deny_by_default: Vec::new(),
            permission_overrides: HashMap::from([("shell".to_string(), PermissionLevel::Full)]),
            default_threshold: PermissionLevel::Read,
            ..Default::default()
        };

        let action = profile.resolve("shell", &serde_json::json!({}));
        assert_eq!(
            action,
            PermissionAction::UseDefaultPermission {
                level: PermissionLevel::Full
            }
        );
    }

    #[test]
    fn test_permission_profile_merge_strictest() {
        let coding = PermissionProfile {
            name: "coding".to_string(),
            auto_approve: HashSet::from(["read_file".to_string(), "grep".to_string()]),
            require_confirmation: HashSet::from(["shell".to_string()]),
            deny_by_default: vec![DenyPattern::deny_tool("shell_dangerous", "Dangerous")],
            permission_overrides: HashMap::from([("shell".to_string(), PermissionLevel::Full)]),
            default_threshold: PermissionLevel::Standard,
            ..Default::default()
        };

        let research = PermissionProfile {
            name: "research".to_string(),
            auto_approve: HashSet::from(["grep".to_string(), "web_search".to_string()]),
            require_confirmation: HashSet::from(["web_fetch".to_string()]),
            deny_by_default: vec![DenyPattern::deny_tool(
                "shell",
                "Research doesn't need shell",
            )],
            permission_overrides: HashMap::from([("shell".to_string(), PermissionLevel::Full)]),
            default_threshold: PermissionLevel::Read,
            ..Default::default()
        };

        let merged = PermissionProfile::merge_strictest(&coding, &research);

        // auto_approve: intersection → only "grep" is approved in both
        assert!(merged.auto_approve.contains("grep"));
        assert!(!merged.auto_approve.contains("read_file")); // Only in coding, not research
        assert!(!merged.auto_approve.contains("web_search")); // Only in research, not coding

        // require_confirmation: union → shell (coding) + web_fetch (research)
        assert!(merged.require_confirmation.contains("shell"));
        assert!(merged.require_confirmation.contains("web_fetch"));

        // deny_by_default: union → both deny patterns
        assert_eq!(merged.deny_by_default.len(), 2);

        // default_threshold: stricter of Standard and Read → Standard
        assert_eq!(merged.default_threshold, PermissionLevel::Standard);
    }

    // ── #28 Stage 4 — ExecPolicy merge + serde round-trip ─────────────────

    fn rule(program: &str, decision: oneai_tool::ExecDecision) -> oneai_tool::ExecRule {
        use oneai_tool::{ExecRule, PatternToken};
        ExecRule {
            pattern: vec![PatternToken::Single(program.into())],
            decision,
            justification: Some("test".into()),
            match_examples: Vec::new(),
            not_match_examples: Vec::new(),
        }
    }

    #[test]
    fn exec_policy_merge_unions_rules_from_both_domains() {
        let mut coding = PermissionProfile::new("coding");
        coding.exec_policy = Some(oneai_tool::ExecPolicy::from_rules(vec![rule(
            "git",
            oneai_tool::ExecDecision::Allow,
        )]));
        let mut research = PermissionProfile::new("research");
        research.exec_policy = Some(oneai_tool::ExecPolicy::from_rules(vec![rule(
            "curl",
            oneai_tool::ExecDecision::Deny,
        )]));

        let merged = PermissionProfile::merge_strictest(&coding, &research);
        let ep = merged.exec_policy.expect("merged policy present");
        assert_eq!(ep.rule_count(), 2);
        assert!(matches!(
            ep.evaluate(&["git".to_string()]),
            Some(oneai_core::Verdict::Allow { .. })
        ));
        assert!(matches!(
            ep.evaluate(&["curl".to_string()]),
            Some(oneai_core::Verdict::Deny { .. })
        ));
    }

    #[test]
    fn exec_policy_merge_one_side_none_takes_other() {
        let mut coding = PermissionProfile::new("coding");
        coding.exec_policy = Some(oneai_tool::ExecPolicy::from_rules(vec![rule(
            "ls",
            oneai_tool::ExecDecision::Allow,
        )]));
        let research = PermissionProfile::new("research"); // exec_policy None

        let merged = PermissionProfile::merge_strictest(&coding, &research);
        let ep = merged.exec_policy.expect("coding's policy carried");
        assert_eq!(ep.rule_count(), 1);
        assert!(matches!(
            ep.evaluate(&["ls".to_string()]),
            Some(oneai_core::Verdict::Allow { .. })
        ));
    }

    #[test]
    fn exec_policy_merge_both_none_yields_none() {
        let coding = PermissionProfile::new("coding");
        let research = PermissionProfile::new("research");
        let merged = PermissionProfile::merge_strictest(&coding, &research);
        assert!(merged.exec_policy.is_none());
    }

    #[test]
    fn exec_policy_serde_round_trip_through_profile() {
        // PermissionProfile serializes ExecPolicy as its rule list; deserialize
        // rebuilds the index via from_rules. Round-trip preserves behaviour.
        let mut profile = PermissionProfile::new("coding");
        profile.exec_policy = Some(oneai_tool::ExecPolicy::from_rules(vec![
            oneai_tool::ExecRule {
                pattern: vec![
                    oneai_tool::PatternToken::Single("git".into()),
                    oneai_tool::PatternToken::Single("commit".into()),
                ],
                decision: oneai_tool::ExecDecision::Allow,
                justification: Some("project rule".into()),
                match_examples: vec!["git commit -m x".into()],
                not_match_examples: vec!["git push".into()],
            },
        ]));

        let json = serde_json::to_string(&profile).expect("serialize");
        let back: PermissionProfile = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(profile, back);
        let ep = back.exec_policy.expect("policy present after round-trip");
        assert_eq!(ep.rule_count(), 1);
        assert!(matches!(
            ep.evaluate(&oneai_tool::shell_tokens("git commit -m x")),
            Some(oneai_core::Verdict::Allow { .. })
        ));
        assert!(ep.evaluate(&oneai_tool::shell_tokens("git push")).is_none());
    }

    #[test]
    fn test_stricter_level() {
        assert_eq!(
            stricter_level(PermissionLevel::Read, PermissionLevel::Standard),
            PermissionLevel::Standard
        );
        assert_eq!(
            stricter_level(PermissionLevel::Standard, PermissionLevel::Full),
            PermissionLevel::Full
        );
        assert_eq!(
            stricter_level(PermissionLevel::Read, PermissionLevel::Read),
            PermissionLevel::Read
        );
    }
}
