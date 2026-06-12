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

use serde::{Deserialize, Serialize};
use oneai_core::PermissionLevel;

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

/// The action determined by PermissionProfile for a tool call.
///
/// This is the result of resolving the 5-step permission decision chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionAction {
    /// Always deny this tool call — matches a deny pattern.
    Deny { reason: String },

    /// Auto-approve this tool call — skip the approval gate entirely.
    AutoApprove,

    /// Require human confirmation — route through the approval gate.
    RequireConfirmation,

    /// No domain-specific rule — fall back to the tool's own risk_level().
    UseDefaultPermission { level: PermissionLevel },
}

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
                return PermissionAction::Deny { reason: pattern.reason.clone() };
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
        PermissionAction::UseDefaultPermission { level: self.default_threshold }
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
        let auto_approve = a.auto_approve.intersection(&b.auto_approve).cloned().collect();

        // Require confirmation: union (confirmed in ANY domain)
        let require_confirmation = a.require_confirmation.union(&b.require_confirmation).cloned().collect();

        // Deny patterns: union
        let deny_by_default = a.deny_by_default.iter().cloned()
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

        Self {
            name,
            auto_approve,
            require_confirmation,
            deny_by_default,
            permission_overrides,
            default_threshold,
        }
    }
}

impl Default for PermissionProfile {
    fn default() -> Self {
        Self::new("default")
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
            deny_by_default: vec![DenyPattern::deny_tool_args("shell", "rm.*-rf", "Root deletion")],
            permission_overrides: HashMap::new(),
            default_threshold: PermissionLevel::Standard,
        };

        let action = profile.resolve("shell", &serde_json::json!({"command": "rm -rf /"}));
        assert_eq!(action, PermissionAction::Deny { reason: "Root deletion".to_string() });
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
        };

        let action = profile.resolve("shell", &serde_json::json!({}));
        assert_eq!(action, PermissionAction::UseDefaultPermission { level: PermissionLevel::Full });
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
        };

        let research = PermissionProfile {
            name: "research".to_string(),
            auto_approve: HashSet::from(["grep".to_string(), "web_search".to_string()]),
            require_confirmation: HashSet::from(["web_fetch".to_string()]),
            deny_by_default: vec![DenyPattern::deny_tool("shell", "Research doesn't need shell")],
            permission_overrides: HashMap::from([("shell".to_string(), PermissionLevel::Full)]),
            default_threshold: PermissionLevel::Read,
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

    #[test]
    fn test_stricter_level() {
        assert_eq!(stricter_level(PermissionLevel::Read, PermissionLevel::Standard), PermissionLevel::Standard);
        assert_eq!(stricter_level(PermissionLevel::Standard, PermissionLevel::Full), PermissionLevel::Full);
        assert_eq!(stricter_level(PermissionLevel::Read, PermissionLevel::Read), PermissionLevel::Read);
    }
}
