// worker_mmr.rs — Maximal Marginal Relevance reranking for recall.
//
// The dilution problem this fixes:
// When a semantic memory is consolidated from episodics, all three live
// near each other in embedding space (because the semantic was distilled
// from those episodics — they share the meaning). Top-K recall by raw
// cosine similarity then returns the semantic AND the episodics it was
// distilled from, eating three result slots with what is structurally
// one piece of knowledge.
//
// MMR fixes this at recall time by penalising candidates that are too
// similar to memories already selected for this result set. The
// parent-child relationship lives in embedding space already; MMR is
// the structurally correct selector for "the same meaning, presented
// once."
//
// Algorithm: at each step, pick the remaining candidate that maximises
//     λ · sim(query, candidate)  −  (1 − λ) · max_{s ∈ selected} sim(candidate, s)
//
//   λ = 1.0  → pure relevance (degenerates to top-K cosine)
//   λ = 0.0  → pure diversity (ignores query)
//   λ = 0.7  → 70% relevance, 30% diversity. Current default.
//
// Cost: O(k · n · d) where n = candidate count, d = embedding dim.
// For n=40, k=10, d=768 that's ~300k float ops per recall — negligible.

#![cfg(target_family = "wasm")]

use crate::hybrid::cosine_similarity;
use crate::worker_vectorize::VectorMatchWithVector;

/// Rerank a candidate set (already roughly relevance-ordered from
/// Vectorize) to balance relevance against diversity. Returns ids of
/// the top `k` MMR-selected matches in the order they were selected
/// (selection order ≈ best result first).
///
/// The first pick is always the highest-relevance candidate: MMR with
/// an empty selected set degenerates to argmax-relevance.
pub fn mmr_rerank(
    query_emb: &[f64],
    candidates: &[VectorMatchWithVector],
    k: usize,
    lambda: f64,
) -> Vec<String> {
    let k = k.min(candidates.len());
    if k == 0 {
        return Vec::new();
    }

    // Precompute query-candidate similarities. Vectorize also reports
    // these as `score`, but recomputing from the returned vectors keeps
    // MMR self-consistent against any future where the index uses a
    // different distance metric than cosine.
    let rel: Vec<f64> = candidates
        .iter()
        .map(|c| cosine_similarity(query_emb, &c.values))
        .collect();

    let mut selected: Vec<usize> = Vec::with_capacity(k);
    let mut remaining: Vec<usize> = (0..candidates.len()).collect();

    while selected.len() < k && !remaining.is_empty() {
        let mut best_pos = 0usize;
        let mut best_score = f64::NEG_INFINITY;

        for (pos, &cand_idx) in remaining.iter().enumerate() {
            let relevance = rel[cand_idx];
            // Penalty: the candidate's similarity to whichever already-
            // selected memory it's closest to. Empty selected → 0.
            let max_sim_to_selected = selected
                .iter()
                .map(|&s| {
                    cosine_similarity(&candidates[s].values, &candidates[cand_idx].values)
                })
                .fold(0.0_f64, f64::max);

            let mmr_score = lambda * relevance - (1.0 - lambda) * max_sim_to_selected;
            if mmr_score > best_score {
                best_score = mmr_score;
                best_pos = pos;
            }
        }

        let chosen = remaining.swap_remove(best_pos);
        selected.push(chosen);
    }

    selected
        .into_iter()
        .map(|i| candidates[i].id.clone())
        .collect()
}
