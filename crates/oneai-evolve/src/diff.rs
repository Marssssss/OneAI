//! `diff.rs` — E5 `evolve diff`: a structured config diff between the seed
//! pack and a generation's frontier config. Powers `oneai evolve diff
//! <run-dir>` so an operator can see exactly what the variation loop changed
//! (which axes, which values) without diffing two JSON blobs by eye.
//!
//! Only **changed** fields appear (additions/removals for Vec/HashMap;
//! before→after for scalars). The axes mirror the variation substrate
//! (`DomainPackConfig`'s layers — design §3.0 变异基质全图): system_prompt,
//! tool_decorators, tools, permission_profile, compression_template,
//! memory_profile (recall.top_k + extraction_schema), context_sources,
//! paradigm_strategies. Numeric-only diffs (recall.top_k) are flagged so a
//! reader knows replay was eligible for that frontier (design §6.4).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use oneai_core::RecallConfig;
use oneai_domain::{
    CompressionTemplateConfig, DomainPackConfig, MemoryProfileConfig, PermissionProfileConfig,
};

use crate::subgraph::ParamRef;

/// One changed field in the config diff.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum DiffEntry {
    /// A scalar field replaced (before → after).
    #[serde(rename = "set")]
    Set {
        path: String,
        before: String,
        after: String,
    },
    /// A list/HashMap entry was added.
    #[serde(rename = "add")]
    Add { path: String, value: String },
    /// A list/HashMap entry was removed.
    #[serde(rename = "remove")]
    Remove { path: String, value: String },
}

/// The structured diff between a seed and a frontier config.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ConfigDiff {
    /// One entry per changed field, in a stable order (system_prompt →
    /// tools → decorators → permission → compression → memory → context →
    /// paradigm).
    pub entries: Vec<DiffEntry>,
    /// True iff every change is a numeric-axis mutation (recall.top_k /
    /// core_budget_tokens / loop-overlay numerics) — i.e. the frontier is
    /// replay-eligible per design §6.4 (no free-text / list mutation).
    pub numeric_only: bool,
}

impl ConfigDiff {
    /// Whether the diff is empty (frontier == seed).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Render the diff as a compact human-readable markdown block (for `evolve
    /// diff`'s default output).
    pub fn to_markdown(&self) -> String {
        if self.entries.is_empty() {
            return "no changes — frontier == seed\n".to_string();
        }
        let mut s = String::new();
        for e in &self.entries {
            match e {
                DiffEntry::Set {
                    path,
                    before,
                    after,
                } => {
                    s.push_str(&format!("- {path}: {before} → {after}\n"));
                }
                DiffEntry::Add { path, value } => {
                    s.push_str(&format!("- {path}: + {value}\n"));
                }
                DiffEntry::Remove { path, value } => {
                    s.push_str(&format!("- {path}: - {value}\n"));
                }
            }
        }
        if self.numeric_only {
            s.push_str("\n(numeric-only mutation — replay-eligible)\n");
        }
        s
    }
}

/// Compute the structured diff between a seed config and a frontier config.
/// The frontier is the candidate; the seed is the baseline. Only changed
/// fields appear.
pub fn config_diff(seed: &DomainPackConfig, frontier: &DomainPackConfig) -> ConfigDiff {
    let mut entries = Vec::new();

    // system_prompt
    if seed.system_prompt != frontier.system_prompt {
        entries.push(DiffEntry::Set {
            path: ParamRef::PackSystemPrompt.path(),
            before: seed.system_prompt.clone(),
            after: frontier.system_prompt.clone(),
        });
    }

    // tools (Vec<String>)
    diff_vec(
        &seed.tools,
        &frontier.tools,
        ParamRef::PackTool(String::new()).path().replace(']', ""),
        &mut entries,
        "tools",
    );

    // tool_decorators (HashMap<String,String>)
    diff_hashmap(
        &seed.tool_decorators,
        &frontier.tool_decorators,
        "pack.tool_decorators",
        &mut entries,
    );

    // permission_profile
    diff_permission(
        &seed.permission_profile,
        &frontier.permission_profile,
        &mut entries,
    );

    // compression_template
    if seed.compression_template != frontier.compression_template {
        diff_compression(
            &seed.compression_template,
            &frontier.compression_template,
            &mut entries,
        );
    }

    // memory_profile
    diff_memory(&seed.memory_profile, &frontier.memory_profile, &mut entries);

    // context_sources
    diff_vec_str(
        &seed.context_sources,
        &frontier.context_sources,
        "pack.context_sources",
        &mut entries,
    );

    // paradigm_strategies — coarse: only flag count change (deep diff of
    // regex/sequence is noisy + low-value for an operator scan).
    if seed.paradigm_strategies.len() != frontier.paradigm_strategies.len() {
        entries.push(DiffEntry::Set {
            path: "pack.paradigm_strategies".to_string(),
            before: seed.paradigm_strategies.len().to_string(),
            after: frontier.paradigm_strategies.len().to_string(),
        });
    }

    let numeric_only = is_numeric_only_diff(&entries);
    ConfigDiff {
        entries,
        numeric_only,
    }
}

/// Diff two `Vec<String>` keyed by element (used for `tools`).
fn diff_vec(
    seed: &[String],
    frontier: &[String],
    _path_prefix: String,
    entries: &mut Vec<DiffEntry>,
    _label: &str,
) {
    for t in frontier {
        if !seed.iter().any(|s| s == t) {
            entries.push(DiffEntry::Add {
                path: format!("pack.tools[{t}]"),
                value: t.clone(),
            });
        }
    }
    for t in seed {
        if !frontier.iter().any(|s| s == t) {
            entries.push(DiffEntry::Remove {
                path: format!("pack.tools[{t}]"),
                value: t.clone(),
            });
        }
    }
}

/// Diff two `Vec<String>` with a flat path prefix (used for
/// `context_sources`).
fn diff_vec_str(seed: &[String], frontier: &[String], path: &str, entries: &mut Vec<DiffEntry>) {
    for t in frontier {
        if !seed.iter().any(|s| s == t) {
            entries.push(DiffEntry::Add {
                path: path.to_string(),
                value: t.clone(),
            });
        }
    }
    for t in seed {
        if !frontier.iter().any(|s| s == t) {
            entries.push(DiffEntry::Remove {
                path: path.to_string(),
                value: t.clone(),
            });
        }
    }
}

/// Diff two `HashMap<String,String>` (used for `tool_decorators`).
fn diff_hashmap(
    seed: &HashMap<String, String>,
    frontier: &HashMap<String, String>,
    path_prefix: &str,
    entries: &mut Vec<DiffEntry>,
) {
    for (k, v) in frontier {
        match seed.get(k) {
            Some(sv) if sv == v => {}
            Some(_) => entries.push(DiffEntry::Set {
                path: format!("{path_prefix}[{k}]"),
                before: seed.get(k).cloned().unwrap_or_default(),
                after: v.clone(),
            }),
            None => entries.push(DiffEntry::Add {
                path: format!("{path_prefix}[{k}]"),
                value: v.clone(),
            }),
        }
    }
    for (k, v) in seed {
        if !frontier.contains_key(k) {
            entries.push(DiffEntry::Remove {
                path: format!("{path_prefix}[{k}]"),
                value: v.clone(),
            });
        }
    }
}

/// Diff two permission profiles — flag any tool that moved tiers
/// (auto_approve ↔ require_confirmation ↔ deny_by_default).
fn diff_permission(
    seed: &PermissionProfileConfig,
    frontier: &PermissionProfileConfig,
    entries: &mut Vec<DiffEntry>,
) {
    use std::collections::HashSet;
    let seed_auto: HashSet<&str> = seed.auto_approve.iter().map(|s| s.as_str()).collect();
    let seed_req: HashSet<&str> = seed
        .require_confirmation
        .iter()
        .map(|s| s.as_str())
        .collect();
    let seed_deny: HashSet<&str> = seed
        .deny_by_default
        .iter()
        .map(|d| d.tool.as_str())
        .collect();
    let fr_auto: HashSet<&str> = frontier.auto_approve.iter().map(|s| s.as_str()).collect();
    let fr_req: HashSet<&str> = frontier
        .require_confirmation
        .iter()
        .map(|s| s.as_str())
        .collect();
    let fr_deny: HashSet<&str> = frontier
        .deny_by_default
        .iter()
        .map(|d| d.tool.as_str())
        .collect();
    // Auto-approve changes.
    for t in fr_auto.iter() {
        if !seed_auto.contains(t) {
            entries.push(DiffEntry::Add {
                path: "pack.permission_profile.auto_approve".to_string(),
                value: t.to_string(),
            });
        }
    }
    for t in seed_auto.iter() {
        if !fr_auto.contains(t) {
            entries.push(DiffEntry::Remove {
                path: "pack.permission_profile.auto_approve".to_string(),
                value: t.to_string(),
            });
        }
    }
    for t in fr_req.iter() {
        if !seed_req.contains(t) {
            entries.push(DiffEntry::Add {
                path: "pack.permission_profile.require_confirmation".to_string(),
                value: t.to_string(),
            });
        }
    }
    for t in seed_req.iter() {
        if !fr_req.contains(t) {
            entries.push(DiffEntry::Remove {
                path: "pack.permission_profile.require_confirmation".to_string(),
                value: t.to_string(),
            });
        }
    }
    for t in fr_deny.iter() {
        if !seed_deny.contains(t) {
            entries.push(DiffEntry::Add {
                path: "pack.permission_profile.deny_by_default".to_string(),
                value: t.to_string(),
            });
        }
    }
    for t in seed_deny.iter() {
        if !fr_deny.contains(t) {
            entries.push(DiffEntry::Remove {
                path: "pack.permission_profile.deny_by_default".to_string(),
                value: t.to_string(),
            });
        }
    }
}

/// Diff two compression templates.
fn diff_compression(
    seed: &CompressionTemplateConfig,
    frontier: &CompressionTemplateConfig,
    entries: &mut Vec<DiffEntry>,
) {
    if seed.name != frontier.name {
        entries.push(DiffEntry::Set {
            path: "pack.compression_template.name".to_string(),
            before: seed.name.clone(),
            after: frontier.name.clone(),
        });
    }
    diff_vec_str(
        &seed.preserve_fields,
        &frontier.preserve_fields,
        "pack.compression_template.preserve_fields",
        entries,
    );
    // truncate_rules (HashMap<String, usize>) — diff by key.
    for (k, v) in &frontier.truncate_rules {
        match seed.truncate_rules.get(k) {
            Some(sv) if sv == v => {}
            Some(_) => entries.push(DiffEntry::Set {
                path: format!("pack.compression_template.truncate_rules[{k}]"),
                before: seed
                    .truncate_rules
                    .get(k)
                    .map(|n| n.to_string())
                    .unwrap_or_default(),
                after: v.to_string(),
            }),
            None => entries.push(DiffEntry::Add {
                path: format!("pack.compression_template.truncate_rules[{k}]"),
                value: v.to_string(),
            }),
        }
    }
    for (k, v) in &seed.truncate_rules {
        if !frontier.truncate_rules.contains_key(k) {
            entries.push(DiffEntry::Remove {
                path: format!("pack.compression_template.truncate_rules[{k}]"),
                value: v.to_string(),
            });
        }
    }
}

/// Diff two memory profiles (recall + extraction_schema + core_budget).
fn diff_memory(
    seed: &MemoryProfileConfig,
    frontier: &MemoryProfileConfig,
    entries: &mut Vec<DiffEntry>,
) {
    if seed.recall != frontier.recall {
        diff_recall(&seed.recall, &frontier.recall, entries);
    }
    if seed.core_budget_tokens != frontier.core_budget_tokens {
        entries.push(DiffEntry::Set {
            path: "pack.memory.core_budget_tokens".to_string(),
            before: seed.core_budget_tokens.to_string(),
            after: frontier.core_budget_tokens.to_string(),
        });
    }
    diff_vec_str(
        &seed.extraction_schema,
        &frontier.extraction_schema,
        "pack.memory.extraction_schema",
        entries,
    );
}

/// Diff two recall configs.
fn diff_recall(seed: &RecallConfig, frontier: &RecallConfig, entries: &mut Vec<DiffEntry>) {
    if seed.strategy != frontier.strategy {
        entries.push(DiffEntry::Set {
            path: "pack.memory.recall.strategy".to_string(),
            before: format!("{:?}", seed.strategy),
            after: format!("{:?}", frontier.strategy),
        });
    }
    if seed.top_k != frontier.top_k {
        entries.push(DiffEntry::Set {
            path: ParamRef::PackRecallTopK.path(),
            before: seed.top_k.to_string(),
            after: frontier.top_k.to_string(),
        });
    }
    if seed.time_decay != frontier.time_decay {
        entries.push(DiffEntry::Set {
            path: "pack.memory.recall.time_decay".to_string(),
            before: format!("{:?}", seed.time_decay),
            after: format!("{:?}", frontier.time_decay),
        });
    }
}

/// Whether every diff entry is on a numeric axis (recall.top_k /
/// core_budget_tokens) — the replay-eligible subset per design §6.4.
fn is_numeric_only_diff(entries: &[DiffEntry]) -> bool {
    if entries.is_empty() {
        return false;
    }
    entries.iter().all(|e| match e {
        DiffEntry::Set { path, .. } => {
            path == "pack.memory.recall.top_k" || path == "pack.memory.core_budget_tokens"
        }
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oneai_core::{RecallConfig, RecallStrategy};
    use oneai_domain::{DomainPackConfig, PermissionProfileConfig};

    fn seed() -> DomainPackConfig {
        DomainPackConfig {
            name: "seed".to_string(),
            description: String::new(),
            tools: vec!["read_file".to_string()],
            tool_decorators: HashMap::new(),
            context_sources: vec![],
            permission_profile: PermissionProfileConfig {
                auto_approve: vec!["read_file".to_string()],
                require_confirmation: vec![],
                deny_by_default: vec![],
            },
            paradigm_strategies: vec![],
            compression_template: Default::default(),
            system_prompt: "You are a coding agent.".to_string(),
            memory_profile: MemoryProfileConfig {
                recall: RecallConfig {
                    strategy: RecallStrategy::Hybrid,
                    top_k: 5,
                    ..Default::default()
                },
                core_budget_tokens: 4096,
                extraction_schema: vec![],
                ..Default::default()
            },
        }
    }

    #[test]
    fn diff_empty_when_identical() {
        let s = seed();
        let d = config_diff(&s, &s);
        assert!(d.is_empty());
        assert!(!d.numeric_only);
    }

    #[test]
    fn diff_numeric_only_recall_top_k() {
        let s = seed();
        let mut f = s.clone();
        f.memory_profile.recall.top_k = 8;
        let d = config_diff(&s, &f);
        assert_eq!(d.entries.len(), 1);
        assert!(d.numeric_only, "recall.top_k change is numeric-only");
        let markdown = d.to_markdown();
        assert!(markdown.contains("pack.memory.recall.top_k: 5 → 8"));
        assert!(markdown.contains("replay-eligible"));
    }

    #[test]
    fn diff_semantic_system_prompt_not_numeric_only() {
        let s = seed();
        let mut f = s.clone();
        f.system_prompt = "Answer with just the number.".to_string();
        let d = config_diff(&s, &f);
        assert_eq!(d.entries.len(), 1);
        assert!(!d.numeric_only, "system_prompt change is semantic");
        assert!(!d.to_markdown().contains("replay-eligible"));
    }

    #[test]
    fn diff_tool_add_remove() {
        let s = seed();
        let mut f = s.clone();
        f.tools.push("calculator".to_string());
        let d = config_diff(&s, &f);
        assert!(d.entries.iter().any(|e| matches!(
            e,
            DiffEntry::Add { value, .. } if value == "calculator"
        )));
        assert!(!d.numeric_only);
    }

    #[test]
    fn diff_permission_widening_detected() {
        let s = seed();
        let mut f = s.clone();
        // Frontier auto-approves a tool the seed didn't — flagged.
        f.permission_profile.auto_approve.push("shell".to_string());
        let d = config_diff(&s, &f);
        assert!(d.entries.iter().any(|e| matches!(
            e,
            DiffEntry::Add { path, value } if path.contains("auto_approve") && value == "shell"
        )));
    }
}
