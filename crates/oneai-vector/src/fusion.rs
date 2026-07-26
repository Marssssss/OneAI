//! Fusion helpers for multi-leg retrieval.
//!
//! Both operate over `(id, score)` legs — each leg is a ranked list of
//! `(String id, f32 score)` tuples (the score is leg-specific and not
//! normalized across legs). The output is a single ranked list of
//! `(id, fused_score)` tuples, sorted descending.
//!
//! [`rrf_fuse`] implements Reciprocal Rank Fusion (Cormack et al. 2009):
//! `fused = Σ w_i / (k + rank_i)`, with `k` defaulting to 60 (the empirical
//! constant used by Weaviate/Milvus and Anthropic's reference pipeline).
//!
//! [`dbsf_fuse`] implements Distribution-Based Score Fusion (Qdrant v1.11):
//! 3-sigma normalize each leg's raw scores then sum. Only prefer DBSF when
//! retrievers are well-calibrated and an eval set confirms it beats weighted
//! RRF — RRF is the default.

use std::collections::HashMap;

/// Fuse multiple ranked legs via Reciprocal Rank Fusion.
///
/// `k` is the RRF constant (use 60 for the Cormack 2009 default). `weights`
/// optionally biases legs (aligned with `legs` order; defaults to 1.0 each).
/// Returns `(id, fused_score)` tuples sorted by fused score descending.
pub fn rrf_fuse(legs: &[Vec<(String, f32)>], k: u32, weights: Option<&[f32]>) -> Vec<(String, f32)> {
    let k = k.max(1) as f32;
    let w: Vec<f32> = match weights {
        Some(ws) if ws.len() == legs.len() => ws.to_vec(),
        _ => vec![1.0; legs.len()],
    };
    let mut acc: HashMap<String, f32> = HashMap::new();
    for (leg_idx, leg) in legs.iter().enumerate() {
        let weight = w[leg_idx];
        // rank is 1-based; ties in score share the better rank (stable order
        // as emitted by the backend).
        for (rank, (id, _score)) in leg.iter().enumerate() {
            let rank = (rank + 1) as f32;
            *acc.entry(id.clone()).or_insert(0.0) += weight / (k + rank);
        }
    }
    let mut out: Vec<(String, f32)> = acc.into_iter().collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// Fuse multiple ranked legs via Distribution-Based Score Fusion.
///
/// Per leg: subtract the mean and divide by 3σ (clamped to a tiny floor so a
/// constant-score leg doesn't divide by zero), which maps ~99.7% of scores
/// into [-1, 1]; then sum the normalized scores across legs. Unlike RRF this
/// uses the raw score magnitudes, so it only wins when both retrievers are
/// well-calibrated.
pub fn dbsf_fuse(legs: &[Vec<(String, f32)>]) -> Vec<(String, f32)> {
    let mut normalized: Vec<Vec<(String, f32)>> = Vec::with_capacity(legs.len());
    for leg in legs {
        if leg.is_empty() {
            normalized.push(Vec::new());
            continue;
        }
        let n = leg.len() as f32;
        let mean = leg.iter().map(|(_, s)| s).sum::<f32>() / n;
        let var = leg.iter().map(|(_, s)| (s - mean).powi(2)).sum::<f32>() / n;
        let sigma3 = (3.0 * var).sqrt().max(1e-6);
        normalized.push(
            leg.iter()
                .map(|(id, s)| (id.clone(), (s - mean) / sigma3))
                .collect(),
        );
    }
    let mut acc: HashMap<String, f32> = HashMap::new();
    for leg in &normalized {
        for (id, s) in leg {
            *acc.entry(id.clone()).or_insert(0.0) += s;
        }
    }
    let mut out: Vec<(String, f32)> = acc.into_iter().collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_rank_fusion_basic() {
        // a is rank-1 in both legs; b is rank-2 in both → a wins unambiguously.
        let a = vec![("a".into(), 1.0), ("b".into(), 0.9), ("c".into(), 0.8)];
        let b = vec![("a".into(), 5.0), ("b".into(), 4.0), ("d".into(), 3.0)];
        let fused = rrf_fuse(&[a, b], 60, None);
        assert_eq!(fused[0].0, "a");
        assert_eq!(fused[1].0, "b");
        assert!(fused[0].1 > fused[1].1, "rank-1-in-both must beat rank-2-in-both");
        // c and d each appear in exactly one leg, at rank 3 → tied at the bottom.
        assert_eq!(fused.len(), 4);
        assert!((fused[2].1 - fused[3].1).abs() < 1e-9, "single-leg rank-3 ties");
        let last_two: Vec<&str> = fused[2..].iter().map(|(id, _)| id.as_str()).collect();
        assert!(last_two.contains(&"c") && last_two.contains(&"d"));
    }

    #[test]
    fn rrf_weights_bias_leg() {
        let a = vec![("x".into(), 1.0), ("y".into(), 0.5)];
        let b = vec![("y".into(), 1.0), ("x".into(), 0.5)];
        // equal weight → x and y tie; weight leg B 10x → y wins
        let tied = rrf_fuse(&[a.clone(), b.clone()], 60, None);
        assert!((tied[0].1 - tied[1].1).abs() < 1e-9);
        let biased = rrf_fuse(&[a, b], 60, Some(&[1.0, 10.0]));
        assert_eq!(biased[0].0, "y");
    }

    #[test]
    fn dbsf_uses_score_magnitude() {
        // DBSF normalizes each leg to zero-mean / 3-sigma, so a single leg
        // orders docs by their (normalized) score: top > 0 > bottom.
        let a = vec![("p".into(), 1.0), ("q".into(), 0.5), ("r".into(), 0.0)];
        let fused = dbsf_fuse(&[a]);
        assert_eq!(fused[0].0, "p");
        assert_eq!(fused[2].0, "r");
    }
}
