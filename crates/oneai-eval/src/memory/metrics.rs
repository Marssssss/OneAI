//! Pure-Rust memory metrics (no LLM, CI-runnable).
//!
//! - `recall_at_k` / `ndcg_at_k`: retrieval quality against gold evidence
//!   keys (LoCoMo / LongMemEval-style, computable because cases carry
//!   annotated evidence labels).
//! - `f1_partial`: token-level lexical F1 (LoCoMo partial-match).
//! - `bleu1`: unigram precision (Mem0 reports this alongside F1).

use std::collections::HashSet;

use crate::eval_metric::EvalScore;

/// Evidence key = `(subject, predicate)`.
type Key = (String, String);

/// Recall@k: fraction of gold evidence keys present in the retrieved set's
/// top-k. `|retrieved ∩ gold| / |gold|`. 1.0 when all gold keys surface.
pub fn recall_at_k(retrieved: &[Key], gold: &[Key], k: usize) -> f64 {
    if gold.is_empty() {
        return 1.0; // nothing to retrieve (e.g. abstention).
    }
    let top: HashSet<&Key> = retrieved.iter().take(k).collect();
    let hit = gold.iter().filter(|g| top.contains(g)).count();
    hit as f64 / gold.len() as f64
}

/// NDCG@k over the retrieved ranking vs gold. A gold key at rank 0 contributes
/// more than one at rank k-1 (graded relevance = 1 for gold, 0 otherwise).
pub fn ndcg_at_k(retrieved: &[Key], gold: &[Key], k: usize) -> f64 {
    let gold_set: HashSet<&Key> = gold.iter().collect();
    if gold.is_empty() {
        return 1.0;
    }
    let dcg: f64 = retrieved.iter().take(k).enumerate()
        .map(|(i, r)| {
            if gold_set.contains(r) {
                1.0 / ((i as f64) + 2.0).ln()
            } else {
                0.0
            }
        })
        .sum();
    // Ideal DCG: all gold at the top.
    let idcg: f64 = (0..gold.len().min(k))
        .map(|i| 1.0 / ((i as f64) + 2.0).ln())
        .sum();
    if idcg == 0.0 { 0.0 } else { dcg / idcg }
}

/// Tokenize for lexical metrics: lowercase, strip punctuation, split on
/// whitespace. CJK characters are split per-character (so Chinese answers
/// aren't a single giant token).
fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in s.to_lowercase().split_whitespace() {
        let cleaned: String = word.chars()
            .filter(|c| c.is_alphanumeric() || ('\u{4e00}'..='\u{9fff}').contains(c))
            .collect();
        if cleaned.is_empty() {
            continue;
        }
        // Split CJK runs into single chars for fairer overlap.
        let mut buffer = String::new();
        for c in cleaned.chars() {
            if ('\u{4e00}'..='\u{9fff}').contains(&c) {
                if !buffer.is_empty() {
                    out.push(std::mem::take(&mut buffer));
                }
                out.push(c.to_string());
            } else {
                buffer.push(c);
            }
        }
        if !buffer.is_empty() {
            out.push(buffer);
        }
    }
    out
}

/// Token-level F1 with partial matching (LoCoMo-style). Returns precision,
/// recall harmonic mean over multiset token overlap.
pub fn f1_partial(predicted: &str, gold: &str) -> f64 {
    let pred = tokenize(predicted);
    let g = tokenize(gold);
    if g.is_empty() || pred.is_empty() {
        return 0.0;
    }
    let mut g_counts: std::collections::HashMap<&String, usize> = std::collections::HashMap::new();
    for tok in &g { *g_counts.entry(tok).or_insert(0) += 1; }
    let mut overlap = 0usize;
    let mut remaining = g_counts.clone();
    for tok in &pred {
        if let Some(c) = remaining.get_mut(tok) {
            if *c > 0 { *c -= 1; overlap += 1; }
        }
    }
    let precision = overlap as f64 / pred.len() as f64;
    let recall = overlap as f64 / g.len() as f64;
    if precision + recall == 0.0 { 0.0 } else { 2.0 * precision * recall / (precision + recall) }
}

/// BLEU-1: unigram precision with clipping (Mem0 reports this alongside F1).
pub fn bleu1(predicted: &str, gold: &str) -> f64 {
    let pred = tokenize(predicted);
    let g = tokenize(gold);
    if pred.is_empty() || g.is_empty() {
        return 0.0;
    }
    let mut g_counts: std::collections::HashMap<&String, usize> = std::collections::HashMap::new();
    for tok in &g { *g_counts.entry(tok).or_insert(0) += 1; }
    let mut clipped = 0usize;
    let mut remaining = g_counts.clone();
    for tok in &pred {
        if let Some(c) = remaining.get_mut(tok) {
            if *c > 0 { *c -= 1; clipped += 1; }
        }
    }
    clipped as f64 / pred.len() as f64
}

/// Wrap a 0..1 value as a passing EvalScore (pass threshold 0.5).
pub fn score01(metric_name: &str, value: f64, reason: impl Into<String>) -> crate::memory::MemoryMetricOutcome {
    let passed = value >= 0.5;
    crate::memory::MemoryMetricOutcome {
        metric_name: metric_name.to_string(),
        score: EvalScore::new(value, 1.0, reason, passed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_full_and_partial() {
        let g = vec![("a".into(), "x".into()), ("b".into(), "y".into())];
        assert!((recall_at_k(&[("a".into(), "x".into()), ("b".into(), "y".into())], &g, 5) - 1.0).abs() < 1e-9);
        assert!((recall_at_k(&[("a".into(), "x".into())], &g, 5) - 0.5).abs() < 1e-9);
        assert!((recall_at_k(&[], &g, 5)).abs() < 1e-9);
    }

    #[test]
    fn ndcg_ranks_top_hit_higher() {
        let g = vec![("a".into(), "x".into())];
        let top = vec![("a".into(), "x".into()), ("c".into(), "z".into())];
        let bottom = vec![("c".into(), "z".into()), ("a".into(), "x".into())];
        assert!(ndcg_at_k(&top, &g, 5) > ndcg_at_k(&bottom, &g, 5));
    }

    #[test]
    fn f1_and_bleu_identical_answer_is_one() {
        assert!((f1_partial("use pnpm", "use pnpm") - 1.0).abs() < 1e-9);
        assert!((bleu1("use pnpm", "use pnpm") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn f1_handles_chinese_tokens() {
        let s = f1_partial("用户偏好 pnpm", "用户偏好 pnpm");
        assert!((s - 1.0).abs() < 1e-9);
    }
}
