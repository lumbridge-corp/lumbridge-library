//! Shared ranking for both shelves: Reciprocal Rank Fusion of the full-text and vector lists, a small
//! top-rank bonus, then position-aware blending with the reranker (recipe from qmd).
//!
//! Flow used by `docs::search` and `warehouse::search`:
//!   1. FTS top-N and HNSW top-N (same embed model only), each a list of ids best-first.
//!   2. [`rrf`] fuses them (k = 60) and adds +0.05 for a #1 hit in any list, +0.02 for #2–#3.
//!   3. The top ~30 go to the reranker; [`blend`] mixes the fused position with the rerank score:
//!      ranks 1–3 trust retrieval 40/60, ranks 4–10 30/70, the rest 20/80. The reranker has the
//!      majority everywhere, so it can veto a #1 retrieval hit it scores as irrelevant (at the old
//!      75/25 a #1 hit could never fall below 0.75, above any reranker-backed tail hit). Retrieval
//!      still breaks ties among passages the reranker scores alike. Measured on a docs question set
//!      over a private vault (block-[1] accuracy unchanged, MRR 0.891 → 0.897).

use std::collections::HashMap;
use std::hash::Hash;

pub const RRF_K: f32 = 60.0;

/// Fuse ranked id lists. Returns (id, score) best-first. Ties keep first-seen order.
pub fn rrf<K: Eq + Hash + Clone>(lists: &[&[K]]) -> Vec<(K, f32)> {
    let mut score: HashMap<K, f32> = HashMap::new();
    let mut order: Vec<K> = Vec::new();
    for list in lists {
        for (rank, id) in list.iter().enumerate() {
            let bonus = match rank {
                0 => 0.05,
                1 | 2 => 0.02,
                _ => 0.0,
            };
            let e = score.entry(id.clone()).or_insert_with(|| {
                order.push(id.clone());
                0.0
            });
            *e += 1.0 / (RRF_K + rank as f32 + 1.0) + bonus;
        }
    }
    let mut out: Vec<(K, f32)> = order.into_iter().map(|k| {
        let s = score[&k];
        (k, s)
    }).collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    out
}

/// The reranker returns sigmoid probabilities that are often tiny (1e-7 .. 1e-3); compare on logits.
pub fn logit(p: f32) -> f32 {
    // f64: in f32, 1.0 - 1e-9 rounds to 1.0 and a perfect score became +inf → NaN in blend.
    let p = (p as f64).clamp(1e-12, 1.0 - 1e-12);
    (p / (1.0 - p)).ln() as f32
}

/// Min-max normalise to 0..1 (all-equal input maps to 1.0).
pub fn normalise(xs: &[f32]) -> Vec<f32> {
    let (lo, hi) = xs.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(l, h), &x| (l.min(x), h.max(x)));
    if hi.partial_cmp(&lo) != Some(std::cmp::Ordering::Greater) {
        return vec![1.0; xs.len()];
    }
    xs.iter().map(|x| (x - lo) / (hi - lo)).collect()
}

/// Retrieval weight for a candidate at fused position `pos` (0-based); the reranker gets the rest.
pub fn retrieval_weight(pos: usize) -> f32 {
    match pos {
        0..=2 => 0.40,
        3..=9 => 0.30,
        _ => 0.20,
    }
}

/// Re-order candidates. `fused` = normalised fused scores in fused order; `rerank_probs[i]` = the
/// reranker's probability for candidate i (same order). Returns (original index, final score) best-first.
pub fn blend(fused: &[f32], rerank_probs: &[f32]) -> Vec<(usize, f32)> {
    assert_eq!(fused.len(), rerank_probs.len());
    let r = normalise(&rerank_probs.iter().map(|&p| logit(p)).collect::<Vec<_>>());
    mix(fused, &r, 1.0)
}

/// Logit spread (max − min over the reranked head) below which the reranker is treated as unable
/// to separate the candidates: its share of [`blend_logits`] shrinks in proportion.
pub const MIN_LOGIT_SPREAD: f64 = 1.0;

/// [`blend`] on raw reranker logits (from `models::Reranker`, f64) instead of probabilities.
///
/// Why: the server's sigmoid is f32, so every logit above ~16.6 comes back as exactly 1.0 and
/// `blend`'s callers clamp probabilities at 1 − 1e-6 (logit 13.8). On code questions most of the
/// top 30 sit above that (measured: 14–25 of 30 for rmcp / tokio / axum questions, logits up to
/// 35), so they tied and retrieval order decided. The logits themselves still span 8–20 units in
/// the top 10, so ranking on them restores the reranker's signal.
///
/// Saturation guard: if the head's logits span less than [`MIN_LOGIT_SPREAD`] (a reranker that
/// scores everything alike, or a server that only returns clamped probabilities), min-max
/// normalisation would blow that noise up to the full 0..1 range; the reranker weight is scaled by
/// `spread / MIN_LOGIT_SPREAD` instead, so retrieval order decides.
pub fn blend_logits(fused: &[f32], logits: &[f64]) -> Vec<(usize, f32)> {
    assert_eq!(fused.len(), logits.len());
    let (lo, hi) = logits.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(l, h), &x| (l.min(x), h.max(x)));
    let spread = if logits.is_empty() { 0.0 } else { hi - lo };
    let trust = (spread / MIN_LOGIT_SPREAD).clamp(0.0, 1.0) as f32;
    let r: Vec<f32> = if spread > 0.0 { logits.iter().map(|x| ((x - lo) / spread) as f32).collect() } else { vec![1.0; logits.len()] };
    mix(fused, &r, trust)
}

/// Position-aware mix of normalised retrieval scores and normalised rerank scores `r`; `trust`
/// (0..1) scales the reranker's share.
fn mix(fused: &[f32], r: &[f32], trust: f32) -> Vec<(usize, f32)> {
    let f = normalise(fused);
    let mut out: Vec<(usize, f32)> = (0..f.len())
        .map(|i| {
            let w = 1.0 - (1.0 - retrieval_weight(i)) * trust;
            (i, w * f[i] + (1.0 - w) * r[i])
        })
        .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    out
}

/// Cheap token estimate (≈4 chars per token for English and code). Good enough for budgets.
pub fn approx_tokens(s: &str) -> usize {
    s.chars().count().div_ceil(4)
}

/// Keep items best-first while they fit in `max_tokens`: an item that does not fit is skipped and
/// smaller ones after it still get in. The first item is always kept, even if it alone is over.
pub fn take_budget<T>(items: Vec<T>, max_tokens: usize, tokens: impl Fn(&T) -> usize) -> Vec<T> {
    let mut used = 0;
    let mut out = Vec::new();
    for it in items {
        let t = tokens(&it);
        if !out.is_empty() && used + t > max_tokens {
            continue;
        }
        used += t;
        out.push(it);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_rewards_agreement_and_top_rank() {
        let fts = ["a", "b", "c"];
        let vec = ["b", "d", "a"];
        let r = rrf(&[&fts, &vec]);
        assert_eq!(r[0].0, "b"); // #2 and #1
        assert_eq!(r[1].0, "a"); // #1 and #3
        assert_eq!(r.len(), 4);
    }

    #[test]
    fn blend_lets_reranker_promote_a_tail_hit() {
        let fused = [1.0, 0.9, 0.8, 0.5, 0.4, 0.3, 0.2, 0.15, 0.12, 0.1, 0.05, 0.01];
        let mut probs = [1e-6_f32; 12];
        probs[11] = 0.9;
        let b = blend(&fused, &probs);
        let pos = b.iter().position(|(i, _)| *i == 11).unwrap();
        // Everything else is junk to the reranker, so the tail hit goes all the way to the top and
        // the retrieval order decides among the rest.
        assert_eq!(pos, 0, "reranker should lift the tail hit to the top: {b:?}");
        assert_eq!(b[1].0, 0);
    }

    #[test]
    fn reranker_vetoes_an_irrelevant_first_hit() {
        // #1 by retrieval but the reranker calls it junk; #6 is what the reranker believes in.
        let fused = [1.0, 0.8, 0.7, 0.6, 0.5, 0.45, 0.4, 0.3, 0.2, 0.1];
        let mut probs = [1e-4_f32; 10];
        probs[0] = 1e-7;
        probs[5] = 0.99;
        let b = blend(&fused, &probs);
        assert_eq!(b[0].0, 5, "{b:?}");
        assert!(b.iter().find(|(i, _)| *i == 0).unwrap().1 < 0.5, "junk #1 must be able to drop: {b:?}");
        // When the reranker agrees, retrieval order is kept.
        let b = blend(&fused, &[0.9; 10]);
        assert_eq!(b.iter().map(|x| x.0).collect::<Vec<_>>(), (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn perfect_rerank_score_is_finite() {
        assert!(logit(1.0).is_finite() && logit(0.0).is_finite());
        let b = blend(&[1.0, 0.5], &[1.0, 0.0]);
        assert!(b.iter().all(|(_, s)| s.is_finite()));
    }

    #[test]
    fn logits_separate_what_saturated_probabilities_tie() {
        // Measured shape (tokio spawn blocking): the answer is #4 by retrieval with logit 25.8,
        // retrieval's #1..#3 have 16..18; all four are p = 1.0 in f32.
        let fused = [1.0, 0.95, 0.9, 0.85, 0.6, 0.5, 0.4, 0.3];
        let logits: [f64; 8] = [18.2, 16.5, 16.0, 25.8, -3.0, -6.0, -9.0, -12.0];
        let probs: Vec<f32> = logits.iter().map(|&l| (1.0 / (1.0 + (-l).exp())) as f32).map(|p| p.clamp(1e-7, 1.0 - 1e-6)).collect();
        assert_eq!(blend(&fused, &probs)[0].0, 0, "probabilities tie, retrieval wins");
        assert_eq!(blend_logits(&fused, &logits)[0].0, 3, "logits keep the reranker's preference");
    }

    #[test]
    fn flat_logits_fall_back_to_retrieval_order() {
        let fused = [1.0, 0.8, 0.6, 0.4, 0.2];
        // Spread 0.05: noise. Plain min-max would put #5 first.
        let logits = [10.0, 10.01, 10.02, 10.03, 10.05];
        let b = blend_logits(&fused, &logits);
        assert_eq!(b.iter().map(|x| x.0).collect::<Vec<_>>(), vec![0, 1, 2, 3, 4], "{b:?}");
        assert!(blend_logits(&fused, &[3.0; 5]).iter().all(|(_, s)| s.is_finite()));
        assert!(blend_logits(&[], &[]).is_empty());
        // A real spread behaves like `blend` on the same logits.
        let logits: [f64; 5] = [-8.0, -6.0, 2.0, -9.0, -7.0];
        let probs: Vec<f32> = logits.iter().map(|&l| (1.0 / (1.0 + (-l).exp())) as f32).collect();
        let a: Vec<usize> = blend_logits(&fused, &logits).iter().map(|x| x.0).collect();
        let b: Vec<usize> = blend(&fused, &probs).iter().map(|x| x.0).collect();
        assert_eq!(a, b);
    }

    #[test]
    fn budget_keeps_one() {
        let v = take_budget(vec!["x".repeat(4000)], 10, |s| approx_tokens(s));
        assert_eq!(v.len(), 1);
        assert!(take_budget(Vec::<String>::new(), 10, |s| approx_tokens(s)).is_empty());
    }

    #[test]
    fn budget_skips_what_does_not_fit_and_keeps_filling() {
        let v = take_budget(vec![40, 50, 30, 20, 10], 100, |t| *t);
        assert_eq!(v, vec![40, 50, 10]);
        // The first item is kept even when it alone is over budget; later ones must fit.
        let v = take_budget(vec![500, 60, 5], 100, |t| *t);
        assert_eq!(v, vec![500]);
    }
}
