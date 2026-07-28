//! Skill consolidation runner — the LLM half of the curator (Phase 2.1
//! Stage C).
//!
//! [`oneai_skill::SkillCurator::apply_automatic_transitions`] (Stage B) is
//! the *pure-function* half: age-based `Active → Stale → Archived`. This
//! module is the *LLM* half: propose umbrella merges that fold narrow
//! one-session skills into class-level umbrellas, then apply them via the
//! curator's data-layer primitives.
//!
//! **Default-off / opt-in.** Nothing in the `AgentLoop` or the curator's
//! automatic `run` calls this. Only `oneai curator consolidate` (or an
//! equivalent app-level caller) triggers it — that CLI subcommand *is* the
//! opt-in. A full `SubAgentKind::Consolidate` agent loop is a documented
//! future enhancement; Stage C uses a single structured-proposal inference
//! (the reflect sub-agent already owns the multi-step skill-patch path).

use std::collections::HashMap;
use std::path::Path;

use oneai_core::traits::LlmProvider;
use oneai_core::{Conversation, GenerationConfig, InferenceRequest, Message};
use oneai_parser::FuzzyJsonRepair;
use oneai_skill::{MergeReport, SkillCurator};

/// One LLM-proposed umbrella merge (parsed from the model's JSON response).
#[derive(Debug, Clone)]
pub struct MergeProposal {
    pub umbrella_name: String,
    pub umbrella_description: String,
    pub umbrella_body: String,
    pub members: Vec<String>,
}

/// Result of a consolidation pass — what was applied + what was skipped.
#[derive(Debug, Clone, Default)]
pub struct ConsolidationReport {
    /// Merges the curator applied (umbrella created + members archived).
    pub proposals_applied: Vec<MergeReport>,
    /// Proposals skipped entirely (e.g. referenced members, missing skills).
    pub proposals_skipped: Vec<String>,
    /// Whether there were no candidates to consolidate (early-return shape).
    pub empty: bool,
}

/// Run one consolidation pass over the live skill library.
///
/// Builds a Hermes-style consolidation prompt over the candidate skills,
/// asks the provider for a JSON list of merge proposals, parses with the
/// 3-layer [`FuzzyJsonRepair`] defense, and applies each via
/// [`SkillCurator::apply_merge`]. Each merge writes a shared restorable
/// backup before retiring any member — the whole pass is reversible via
/// `oneai curator rollback <id>` on any applied `MergeReport::backup_id`.
pub async fn run_consolidation(
    provider: &dyn LlmProvider,
    curator: &SkillCurator,
    skills_dir: &Path,
    generation: &GenerationConfig,
) -> oneai_core::error::Result<ConsolidationReport> {
    let candidates = curator.consolidation_candidates().await;
    if candidates.len() < 2 {
        // Nothing to merge (an umbrella must cover ≥2 members). Not an error
        // — the library is already consolidated.
        return Ok(ConsolidationReport {
            empty: true,
            ..Default::default()
        });
    }

    let prompt = build_consolidation_prompt(&candidates);
    let raw = infer_consolidation(provider, &prompt, generation).await?;

    let proposals = parse_proposals(&raw);
    if proposals.is_empty() {
        tracing::info!("consolidation: model proposed no merges (raw output had none)");
        return Ok(ConsolidationReport {
            empty: true,
            ..Default::default()
        });
    }

    let mut report = ConsolidationReport::default();
    for proposal in proposals {
        // An umbrella must cover ≥2 members — drop degenerate proposals.
        if proposal.members.len() < 2 {
            report.proposals_skipped.push(format!(
                "umbrella '{}' has <2 members",
                proposal.umbrella_name
            ));
            continue;
        }
        let umbrella = oneai_core::SkillDescriptor {
            name: proposal.umbrella_name.clone(),
            description: proposal.umbrella_description.clone(),
            prompt_template: proposal.umbrella_body.clone(),
            trigger_keywords: Vec::new(),
            embedding: None,
        };
        match curator
            .apply_merge(umbrella, &proposal.members, skills_dir)
            .await
        {
            Ok(mr) => report.proposals_applied.push(mr),
            Err(e) => {
                tracing::warn!(
                    "consolidation: skipping proposal '{}': {e}",
                    proposal.umbrella_name
                );
                report
                    .proposals_skipped
                    .push(format!("{}: {e}", proposal.umbrella_name));
            }
        }
    }
    Ok(report)
}

/// Build the Hermes-style consolidation prompt over the candidate digest.
///
/// Ported from `docs/hermes-pi-inspiration.md` §2.3 Loop B + the Stage A
/// `REFLECT_SYSTEM_PROMPT` preference order: "hundreds of narrow skills each
/// capturing one session is a library failure, not a feature". The model
/// emits a JSON object the runner parses with [`FuzzyJsonRepair`].
fn build_consolidation_prompt(
    candidates: &[(oneai_core::SkillDescriptor, oneai_skill::SkillMetadata)],
) -> String {
    let mut digest = String::new();
    for (s, m) in candidates {
        digest.push_str(&format!(
            "- name: {}\n  description: {}\n  use_count: {}\n  body_excerpt: {}\n",
            s.name,
            s.description,
            m.use_count,
            s.prompt_template.chars().take(240).collect::<String>(),
        ));
    }
    let template = r#"You are a skill-library curator. The agent has accumulated narrow, one-off skills \
that each captured a single session — that is a *library failure, not a feature*. \
Your job is to merge narrow skills into class-level **umbrella** skills that cover \
the shared pattern, so the library stays small and the model can find the one \
skill that governs a whole class of task.

Candidate skills (name / description / use_count / body_excerpt):
{digest}
Rules:
- Propose merges ONLY among the candidates above. Never invent skills not listed.
- Each umbrella must cover **>=2** members. A single-skill "merge" is useless.
- An umbrella name is short, class-level (e.g. `git-workflows`, not `git-push-force-from-feature-branch`).
- The umbrella body folds the members' shared procedure + drops per-session noise.
- Do NOT propose merging a skill the agent clearly still uses standalone \
(use_count high relative to peers) — leave the popular ones alone.
- If no two candidates share a class, return an empty proposals list.

Respond with ONLY a JSON object (no prose, no code fence) of shape:
{{"proposals":[{{"umbrella_name":"...","umbrella_description":"...","umbrella_body":"...","members":["skill-a","skill-b"]}}]}}
"#;
    // {digest} is the one interpolation; the literal JSON braces are escaped
    // as {{ }} so format! doesn't treat them as placeholders.
    template.replace("{digest}", &digest)
}

/// One inference call carrying the consolidation prompt.
async fn infer_consolidation(
    provider: &dyn LlmProvider,
    prompt: &str,
    generation: &GenerationConfig,
) -> oneai_core::error::Result<String> {
    let mut conv = Conversation::new();
    conv.add_message(Message::system(
        "You are a skill-library consolidation engine. You emit ONLY JSON.".to_string(),
    ));
    conv.add_message(Message::user(prompt.to_string()));
    let req = InferenceRequest {
        conversation: conv,
        tools: Vec::new(),
        max_tokens: generation.max_tokens,
        temperature: generation.temperature.or(Some(0.2)),
        top_p: generation.top_p,
        stop_sequences: generation.stop_sequences.clone(),
        constrained_output: None,
        thinking_budget: None,
        metadata: HashMap::new(),
    };
    let resp = provider.infer(req).await?;
    Ok(resp.message.text_content())
}

/// Parse the model's raw text into [`MergeProposal`]s via the 3-layer
/// [`FuzzyJsonRepair`] defense — direct parse → bracket-close repair →
/// regex extraction. Tolerates code fences and surrounding prose.
fn parse_proposals(raw: &str) -> Vec<MergeProposal> {
    let repair = FuzzyJsonRepair::new();
    let value = match repair.repair_and_parse(raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("consolidation: failed to parse model JSON: {e} (raw: {raw})");
            return Vec::new();
        }
    };
    let Some(arr) = value.get("proposals").and_then(|v| v.as_array()) else {
        tracing::warn!("consolidation: response had no 'proposals' array (raw: {raw})");
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in arr {
        let Some(name) = item.get("umbrella_name").and_then(|v| v.as_str()) else {
            continue;
        };
        let members: Vec<String> = item
            .get("members")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        out.push(MergeProposal {
            umbrella_name: name.to_string(),
            umbrella_description: item
                .get("umbrella_description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            umbrella_body: item
                .get("umbrella_body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            members,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock_provider::{MockProvider, ScriptedResponse};
    use oneai_core::SkillDescriptor;
    use oneai_skill::{SkillAuthor, SkillLifecycleConfig, SkillMetadataStore, SkillRegistry};
    use std::sync::Arc;

    fn tmp_root() -> std::path::PathBuf {
        let name = std::thread::current().name().unwrap_or("test").to_string();
        let mut h: u64 = 1469598103934665603;
        for b in name.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        let p = std::env::temp_dir().join(format!("oneai-consol-{h:x}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn make_skill(name: &str, desc: &str) -> SkillDescriptor {
        SkillDescriptor {
            name: name.into(),
            description: desc.into(),
            prompt_template: format!("do {desc} carefully"),
            trigger_keywords: vec!["k".into()],
            embedding: None,
        }
    }

    async fn make_curator(
        skills: Vec<SkillDescriptor>,
    ) -> (
        Arc<SkillRegistry>,
        Arc<SkillMetadataStore>,
        Arc<SkillCurator>,
    ) {
        let registry = Arc::new(SkillRegistry::new());
        for s in &skills {
            registry.register(s.clone()).await.unwrap();
        }
        let store = Arc::new(SkillMetadataStore::new(
            tmp_root(),
            SkillLifecycleConfig::default(),
        ));
        for s in &skills {
            store.ensure(&s.name, SkillAuthor::User, 100).await;
        }
        let curator = Arc::new(SkillCurator::new(
            registry.clone(),
            store.clone(),
            Default::default(),
        ));
        (registry, store, curator)
    }

    #[tokio::test]
    async fn empty_library_returns_early() {
        let (_, _, curator) = make_curator(vec![]).await;
        let provider = MockProvider::always_answers("noop");
        let dir = tmp_root();
        let report = run_consolidation(&provider, &curator, &dir, &GenerationConfig::default())
            .await
            .unwrap();
        assert!(report.empty);
        assert!(report.proposals_applied.is_empty());
    }

    #[tokio::test]
    async fn parses_and_applies_a_proposal() {
        // Two narrow skills that share a class.
        let skills = vec![
            make_skill("git-push-a", "push branch a"),
            make_skill("git-push-b", "push branch b"),
        ];
        let (registry, store, curator) = make_curator(skills).await;

        // Model proposes one umbrella merging both.
        let json = serde_json::json!({
            "proposals": [{
                "umbrella_name": "git-push-workflow",
                "umbrella_description": "push branches safely",
                "umbrella_body": "Always push with lease.",
                "members": ["git-push-a", "git-push-b"]
            }]
        })
        .to_string();
        let provider = MockProvider::from_script(vec![ScriptedResponse::direct_answer(json)]);
        let skills_dir = store.root().join("skills");
        let report = run_consolidation(
            &provider,
            &curator,
            &skills_dir,
            &GenerationConfig::default(),
        )
        .await
        .unwrap();
        assert!(!report.empty);
        assert_eq!(
            report.proposals_applied.len(),
            1,
            "skipped: {:?}",
            report.proposals_skipped
        );
        let mr = &report.proposals_applied[0];
        assert_eq!(mr.umbrella_name, "git-push-workflow");
        assert_eq!(mr.members_archived.len(), 2);
        assert!(registry.find_by_name("git-push-workflow").await.is_some());
        assert!(skills_dir.join("git-push-workflow/SKILL.md").exists());
    }

    #[tokio::test]
    async fn proposal_with_missing_member_skipped() {
        let skills = vec![make_skill("a", "do a"), make_skill("b", "do b")];
        let (registry, store, curator) = make_curator(skills).await;
        // Proposes merging `a` with a non-candidate `c` — apply_merge rejects
        // the missing member, so the whole proposal is skipped (not applied).
        let json = serde_json::json!({
            "proposals": [{
                "umbrella_name": "umb",
                "umbrella_description": "u",
                "umbrella_body": "b",
                "members": ["a", "c"]
            }]
        })
        .to_string();
        let provider = MockProvider::from_script(vec![ScriptedResponse::direct_answer(json)]);
        let skills_dir = store.root().join("skills");
        let report = run_consolidation(
            &provider,
            &curator,
            &skills_dir,
            &GenerationConfig::default(),
        )
        .await
        .unwrap();
        assert!(
            report.proposals_applied.is_empty(),
            "should not apply a proposal with a missing member"
        );
        assert_eq!(report.proposals_skipped.len(), 1);
        // No umbrella written.
        assert!(registry.find_by_name("umb").await.is_none());
    }
}
