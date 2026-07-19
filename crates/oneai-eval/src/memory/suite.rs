//! Builtin synthetic suite (5 abilities) + JSONL loader for external
//! benchmarks (LoCoMo / LongMemEval format).

use std::path::Path;

use super::case::{MemoryAbility, MemoryEvalCase, MemoryEvalSession, PlantedFact};

/// A small planted-fact helper.
fn f(fact_type: &str, subject: &str, predicate: &str, content: &str) -> PlantedFact {
    PlantedFact {
        fact_type: fact_type.to_string(),
        subject: subject.to_string(),
        predicate: predicate.to_string(),
        content: content.to_string(),
        importance: 0.7,
    }
}

fn sess(id: &str, at: &str, facts: Vec<PlantedFact>, msgs: Vec<(&str, &str)>) -> MemoryEvalSession {
    MemoryEvalSession {
        id: id.to_string(),
        at: at.to_string(),
        facts,
        messages: msgs.into_iter().map(|(r, t)| (r.to_string(), t.to_string())).collect(),
    }
}

/// The builtin synthetic suite — ~15 cases covering all 5 LongMemEval
/// abilities, including a deliberate synonym/cross-language anti-example that
/// only semantic recall (§12.1) can solve.
pub fn builtin_suite() -> Vec<MemoryEvalCase> {
    vec![
        // ── IE: single-session extraction ───────────────────────────────
        MemoryEvalCase {
            id: "ie_pkg_manager".into(),
            ability: MemoryAbility::InformationExtraction,
            category: "single_session_user".into(),
            question: "which package manager does the user prefer?".into(),
            gold_answer: "pnpm".into(),
            evidence_keys: vec![("user.package_manager".into(), "prefers".into())],
            sessions: vec![sess("s1", "2026-07-01T10:00:00Z", vec![f("user_tooling_pref", "user.package_manager", "prefers", "pnpm")], vec![
                ("user", "I prefer pnpm as my package manager."),
            ])],
            requires_abstention: false,
            synonym_anti_example: false,
            invalidate_after: vec![],
        },
        // ── IE: synonym anti-example (Chinese fact, English query) ──────
        // Keyword recall has zero byte overlap here; only §12.1 semantic
        // recall surfaces the fact.
        MemoryEvalCase {
            id: "ie_synonym_cross_lang".into(),
            ability: MemoryAbility::InformationExtraction,
            category: "single_session_user_synonym".into(),
            question: "what package manager does the user use to manage dependencies?".into(),
            gold_answer: "pnpm".into(),
            evidence_keys: vec![("用户.包管理器".into(), "偏好".into())],
            sessions: vec![sess("s1", "2026-07-01T10:00:00Z", vec![f("user_tooling_pref", "用户.包管理器", "偏好", "使用 pnpm 管理依赖")], vec![
                ("user", "我喜欢用 pnpm 管理依赖。"),
            ])],
            requires_abstention: false,
            synonym_anti_example: true,
            invalidate_after: vec![],
        },
        // ── MR: combine across two sessions ─────────────────────────────
        MemoryEvalCase {
            id: "mr_project_owner".into(),
            ability: MemoryAbility::MultiSessionReasoning,
            category: "multi_session_reasoning".into(),
            question: "who owns the project the user is building?".into(),
            gold_answer: "Bob owns Project Atlas".into(),
            evidence_keys: vec![
                ("project.name".into(), "is".into()),
                ("project.owner".into(), "is".into()),
            ],
            sessions: vec![
                sess("s1", "2026-07-01T10:00:00Z", vec![f("decision", "project.name", "is", "Project Atlas")], vec![("user", "We started a project called Project Atlas.")]),
                sess("s2", "2026-07-03T10:00:00Z", vec![f("decision", "project.owner", "is", "Bob")], vec![("user", "Bob owns the project.")]),
            ],
            requires_abstention: false,
            synonym_anti_example: false,
            invalidate_after: vec![],
        },
        // ── TR: timestamp-aware ("before week 2") ────────────────────────
        MemoryEvalCase {
            id: "tr_before_week2".into(),
            ability: MemoryAbility::TemporalReasoning,
            category: "temporal_reasoning".into(),
            question: "what was the deployment tool decided on before week 2?".into(),
            gold_answer: "Docker".into(),
            evidence_keys: vec![("deploy.tool".into(), "decided_to".into())],
            sessions: vec![
                sess("s1", "2026-07-01T10:00:00Z", vec![f("decision", "deploy.tool", "decided_to", "Docker")], vec![("user", "Week 1: decided Docker for deployment.")]),
                sess("s2", "2026-07-08T10:00:00Z", vec![f("decision", "deploy.tool", "decided_to", "Kubernetes")], vec![("user", "Week 2: switched to Kubernetes.")]),
            ],
            requires_abstention: false,
            synonym_anti_example: false,
            invalidate_after: vec![],
        },
        // ── KU: knowledge update — old value superseded ─────────────────
        // §12.2 verification: the JWT decision must be invalidated when the
        // user switches to session; recall must return the current value.
        MemoryEvalCase {
            id: "ku_auth_switch".into(),
            ability: MemoryAbility::KnowledgeUpdate,
            category: "knowledge_update".into(),
            question: "what is the current authentication scheme?".into(),
            gold_answer: "session".into(),
            evidence_keys: vec![("auth.scheme".into(), "decided_to".into())],
            sessions: vec![
                sess("s1", "2026-07-01T10:00:00Z", vec![f("decision", "auth.scheme", "decided_to", "JWT")], vec![("user", "Let's use JWT for auth.")]),
                sess("s2", "2026-07-05T10:00:00Z", vec![f("decision", "auth.scheme", "decided_to", "session")], vec![("user", "We dropped JWT and switched to session-based auth.")]),
            ],
            requires_abstention: false,
            synonym_anti_example: false,
            // After s1's fact is planted, invalidate it (s2 plants the new value).
            // (session_index, fact_index) — s1=0, its single fact=0.
            invalidate_after: vec![(0, 0)],
        },
        // ── KU: price change (current value) ─────────────────────────────
        MemoryEvalCase {
            id: "ku_price_current".into(),
            ability: MemoryAbility::KnowledgeUpdate,
            category: "knowledge_update".into(),
            question: "what is the current price of the premium plan?".into(),
            gold_answer: "$30".into(),
            evidence_keys: vec![("plan.premium".into(), "price".into())],
            sessions: vec![
                sess("s1", "2026-07-01T10:00:00Z", vec![f("decision", "plan.premium", "price", "$20")], vec![("user", "Premium is $20 right now.")]),
                sess("s2", "2026-07-10T10:00:00Z", vec![f("decision", "plan.premium", "price", "$30")], vec![("user", "Premium went up to $30.")]),
            ],
            requires_abstention: false,
            synonym_anti_example: false,
            invalidate_after: vec![(0, 0)],
        },
        // ── ABS: abstention — never mentioned ────────────────────────────
        MemoryEvalCase {
            id: "abs_never_mentioned".into(),
            ability: MemoryAbility::Abstention,
            category: "abstention".into(),
            question: "what is the user's phone number?".into(),
            gold_answer: "I don't know".into(),
            evidence_keys: vec![],
            sessions: vec![
                sess("s1", "2026-07-01T10:00:00Z", vec![f("user_tooling_pref", "user.package_manager", "prefers", "pnpm")], vec![("user", "I use pnpm.")]),
            ],
            requires_abstention: true,
            synonym_anti_example: false,
            invalidate_after: vec![],
        },
        // ── ABS: abstention — fact present but unrelated ─────────────────
        MemoryEvalCase {
            id: "abs_unrelated".into(),
            ability: MemoryAbility::Abstention,
            category: "abstention".into(),
            question: "what is the user's home address?".into(),
            gold_answer: "I don't know".into(),
            evidence_keys: vec![],
            sessions: vec![
                sess("s1", "2026-07-01T10:00:00Z", vec![f("decision", "auth.scheme", "decided_to", "JWT")], vec![("user", "We use JWT.")]),
            ],
            requires_abstention: true,
            synonym_anti_example: false,
            invalidate_after: vec![],
        },
        // ── IE: another preference (semantic — paraphrased query) ───────
        MemoryEvalCase {
            id: "ie_test_runner".into(),
            ability: MemoryAbility::InformationExtraction,
            category: "single_session_user".into(),
            question: "which test runner is preferred for running tests?".into(),
            gold_answer: "vitest".into(),
            evidence_keys: vec![("user.test_runner".into(), "prefers".into())],
            sessions: vec![sess("s1", "2026-07-01T10:00:00Z", vec![f("user_tooling_pref", "user.test_runner", "prefers", "vitest")], vec![("user", "I prefer vitest.")])],
            requires_abstention: false,
            synonym_anti_example: false,
            invalidate_after: vec![],
        },
        // ── MR: combine preference + decision ────────────────────────────
        MemoryEvalCase {
            id: "mr_pref_and_decision".into(),
            ability: MemoryAbility::MultiSessionReasoning,
            category: "multi_session_reasoning".into(),
            question: "which package manager and auth scheme has the user chosen?".into(),
            gold_answer: "pnpm and JWT".into(),
            evidence_keys: vec![
                ("user.package_manager".into(), "prefers".into()),
                ("auth.scheme".into(), "decided_to".into()),
            ],
            sessions: vec![
                sess("s1", "2026-07-01T10:00:00Z", vec![f("user_tooling_pref", "user.package_manager", "prefers", "pnpm")], vec![("user", "pnpm please.")]),
                sess("s2", "2026-07-02T10:00:00Z", vec![f("decision", "auth.scheme", "decided_to", "JWT")], vec![("user", "Auth: JWT.")]),
            ],
            requires_abstention: false,
            synonym_anti_example: false,
            invalidate_after: vec![],
        },
    ]
}

/// Load a suite from a JSONL file (one `MemoryEvalCase` per line). Lines that
/// fail to parse are skipped with a `tracing::warn!` so a benchmark with a few
/// exotic cases doesn't abort the run. Returns an empty vec if the file is
/// missing.
pub fn load_suite_jsonl(path: &Path) -> Vec<MemoryEvalCase> {
    let Ok(text) = std::fs::read_to_string(path) else {
        tracing::warn!("Memory eval suite file not found: {}", path.display());
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| match serde_json::from_str::<MemoryEvalCase>(line) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!("Skipping malformed memory eval case: {}", e);
                None
            }
        })
        .collect()
}
