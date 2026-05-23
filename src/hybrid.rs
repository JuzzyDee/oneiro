// hybrid.rs — Hybrid retrieval primitives for recall_check (CLA-109).
//
// Pure logic that's testable on native (worker_mcp.rs and worker_store.rs
// are both wasm-gated). Two pieces here:
//
//   build_fts_query(raw) — user free-text → safe FTS5 MATCH expression.
//                          Whitespace-tokenises, strips non-alphanumeric
//                          edges, quotes each token, OR-joins. Quoting
//                          defends against tokens that happen to be FTS5
//                          keywords (AND, OR, NOT, NEAR) — quoted forms
//                          are literal tokens, not operators.
//
//   rrf_fuse(fts, vector, fts_weight, k) — Reciprocal Rank Fusion of two
//                          ranked id lists. Score per retriever per hit:
//                              weight * 1 / (k + rank)
//                          Summed across retrievers, sorted descending.
//                          k=60 is the original RRF paper's constant
//                          (Cormack et al. 2009); has held up in practice.
//                          RRF doesn't require normalising scores across
//                          retrievers — works directly off rank, which
//                          is why it's the safe default when fusing
//                          systems with incompatible score distributions
//                          (BM25 vs cosine similarity).
//
// Both functions are pure — no I/O, no env access — so they compile on
// every target and live alongside their unit tests.

use std::collections::HashMap;

/// RRF constant from the original paper. Increasing it flattens the
/// score curve (more weight to lower-ranked hits); decreasing it
/// sharpens to favour top ranks. 60 is the established default and
/// rarely benefits from tuning.
pub const DEFAULT_RRF_K: f64 = 60.0;

/// Cosine similarity between two equal-length vectors. Used by MMR
/// rerank (worker_mmr.rs) for the diversity term and by hybrid
/// retrieval (worker_mcp.rs) to score FTS-only candidates after a
/// getByIds vector fetch. Pure function; testable on native.
///
/// Returns 0.0 in the degenerate denominator case rather than NaN —
/// defensive against zero-length vectors. Workers AI / Vectorize never
/// emit a zero embedding for non-empty input, so this branch is
/// unreachable in practice but the safety net is cheap.
pub fn cosine_similarity(a: &[f64], b: &[f64]) -> f64 {
    let mut dot = 0.0;
    let mut na = 0.0;
    let mut nb = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

/// Build a safe FTS5 MATCH expression from user free-text.
///
/// Returns `None` when the input has no usable tokens (empty or pure
/// punctuation) — caller should treat that as "skip the FTS leg of the
/// hybrid query." Returns `Some(expr)` otherwise; `expr` is a properly
/// quoted, OR-joined token list ready to pass directly into a `MATCH ?`
/// bind parameter.
///
/// Tokenisation rules:
///   * split on whitespace
///   * strip leading/trailing non-alphanumeric characters per token
///   * drop empty tokens
///   * strip inner double quotes (defensive — shouldn't reach here)
///   * wrap each surviving token in double quotes
///   * join with `" OR "`
///
/// Quoting each token makes FTS5 treat it as a literal — so a token that
/// happens to be "AND" or "OR" is a search term, not a syntax keyword.
pub fn build_fts_query(raw: &str) -> Option<String> {
    let tokens: Vec<String> = raw
        .split_whitespace()
        .map(|t| {
            t.trim_matches(|c: char| !c.is_alphanumeric())
                .replace('"', "")
        })
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"", t))
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" OR "))
    }
}

/// Reciprocal Rank Fusion of two ranked id lists.
///
/// `fts_ranking` and `vector_ranking` are id lists in rank order (best
/// first, position 0). Each id gets a score from each list it appears
/// in: `weight * 1 / (k + rank_1_indexed)`. Per-list scores sum across
/// retrievers. The output is sorted descending by fused score.
///
/// `fts_weight` scales the lexical leg's contribution. The vector leg
/// is fixed at weight 1.0 (the established baseline). `fts_weight = 0.0`
/// disables FTS entirely (degenerate vector-only ranking). `fts_weight =
/// 1.0` is equal weighting. Anything > 1.0 over-weights lexical.
///
/// Returns `Vec<(id, fused_score)>`. Score is informational — the rank
/// order is what downstream consumers use.
pub fn rrf_fuse(
    fts_ranking: &[String],
    vector_ranking: &[String],
    fts_weight: f64,
    k: f64,
) -> Vec<(String, f64)> {
    let mut scores: HashMap<String, f64> = HashMap::new();
    if fts_weight > 0.0 {
        for (rank, id) in fts_ranking.iter().enumerate() {
            let contribution = fts_weight / (k + (rank + 1) as f64);
            *scores.entry(id.clone()).or_insert(0.0) += contribution;
        }
    }
    for (rank, id) in vector_ranking.iter().enumerate() {
        let contribution = 1.0 / (k + (rank + 1) as f64);
        *scores.entry(id.clone()).or_insert(0.0) += contribution;
    }
    let mut out: Vec<(String, f64)> = scores.into_iter().collect();
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    // ---- build_fts_query ----

    #[test]
    fn empty_input_returns_none() {
        assert!(build_fts_query("").is_none());
        assert!(build_fts_query("   ").is_none());
        assert!(build_fts_query("!!!").is_none()); // pure punctuation
    }

    #[test]
    fn single_token() {
        assert_eq!(build_fts_query("rover").as_deref(), Some("\"rover\""));
    }

    #[test]
    fn multi_token_or_joins() {
        assert_eq!(
            build_fts_query("rover camera heartbeat").as_deref(),
            Some("\"rover\" OR \"camera\" OR \"heartbeat\""),
        );
    }

    #[test]
    fn fts5_keywords_get_quoted_as_literal_tokens() {
        // AND/OR/NOT/NEAR are FTS5 syntax keywords; quoting makes them
        // literal search terms instead of operators.
        let q = build_fts_query("rover AND camera").unwrap();
        assert!(q.contains("\"AND\""));
        assert!(q.contains("\"rover\""));
        assert!(q.contains("\"camera\""));
    }

    #[test]
    fn punctuation_stripped_from_token_edges() {
        // Trailing comma and inner apostrophe are noise; the inner
        // apostrophe is alphanumeric-adjacent so trim_matches leaves
        // it. We only strip edge non-alphanumeric chars.
        let q = build_fts_query("chopper, justin's").unwrap();
        assert!(q.contains("\"chopper\""));
        assert!(q.contains("justin")); // either "justin" or "justin's" — both fine
    }

    #[test]
    fn inner_double_quotes_stripped() {
        // Defensive — a user query containing a literal double quote
        // would otherwise break the FTS5 syntax.
        let q = build_fts_query("say \"hello\"").unwrap();
        assert!(!q.contains("\\\""));
        // The "hello" word survives (without its inner quotes), as a
        // properly-quoted FTS5 token.
        assert!(q.contains("\"hello\""));
    }

    // ---- rrf_fuse ----

    #[test]
    fn empty_inputs_produce_empty_output() {
        assert!(rrf_fuse(&[], &[], 1.0, DEFAULT_RRF_K).is_empty());
    }

    #[test]
    fn single_retriever_orders_by_rank() {
        let fts = ids(&["a", "b", "c"]);
        let out = rrf_fuse(&fts, &[], 1.0, DEFAULT_RRF_K);
        // FTS-only with default weight: order preserved.
        let ordered: Vec<&str> = out.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ordered, vec!["a", "b", "c"]);
    }

    #[test]
    fn overlapping_rankings_boost_shared_hits() {
        // "shared" appears top of both lists; should beat any item that
        // appears in only one.
        let fts = ids(&["shared", "fts_only"]);
        let vec = ids(&["shared", "vec_only"]);
        let out = rrf_fuse(&fts, &vec, 1.0, DEFAULT_RRF_K);
        assert_eq!(out[0].0, "shared");
        // "shared" gets contributions from both retrievers; the others
        // from one each. So its score is roughly double.
        assert!(out[0].1 > out[1].1);
    }

    #[test]
    fn fts_weight_zero_degenerates_to_vector_only() {
        // With fts_weight=0, the FTS leg contributes nothing — the
        // ranking is purely vector's ordering.
        let fts = ids(&["fts_top", "fts_second"]);
        let vec = ids(&["vec_top", "vec_second"]);
        let out = rrf_fuse(&fts, &vec, 0.0, DEFAULT_RRF_K);
        let ordered: Vec<&str> = out.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ordered, vec!["vec_top", "vec_second"]);
    }

    #[test]
    fn fts_weight_higher_than_one_increases_lexical_influence() {
        // Item at rank 0 of FTS, rank 2 of vector vs item at rank 2 of
        // FTS, rank 0 of vector. With fts_weight=1 they'd roughly tie;
        // with fts_weight=3 the FTS-top item should win.
        let fts = ids(&["a", "b", "c"]);
        let vec = ids(&["c", "b", "a"]);
        let balanced = rrf_fuse(&fts, &vec, 1.0, DEFAULT_RRF_K);
        let fts_heavy = rrf_fuse(&fts, &vec, 3.0, DEFAULT_RRF_K);
        // Balanced: a and c tie (mirror rankings); b in the middle.
        // FTS-heavy: a wins outright.
        let _ = balanced; // not asserting balanced tie behaviour — order undefined
        assert_eq!(fts_heavy[0].0, "a");
    }

    #[test]
    fn rrf_is_robust_to_disjoint_rankings() {
        // No overlap between retrievers: every hit appears once.
        let fts = ids(&["a"]);
        let vec = ids(&["b"]);
        let out = rrf_fuse(&fts, &vec, 1.0, DEFAULT_RRF_K);
        assert_eq!(out.len(), 2);
        // Both at rank 0 with equal weights → equal scores.
        assert!((out[0].1 - out[1].1).abs() < 1e-12);
    }

    #[test]
    fn rrf_k_constant_affects_rank_curve() {
        // Higher k flattens the score curve. With k=1, rank-1 hits get
        // 1/2 = 0.5; rank-10 gets 1/11 = 0.09 — sharp.
        // With k=100, rank-1 gets 1/101 ≈ 0.0099; rank-10 gets 1/110 ≈
        // 0.0091 — much flatter.
        let fts = ids(&["a"]);
        let vec: Vec<String> = (0..10).map(|i| format!("v{}", i)).collect();
        let sharp = rrf_fuse(&fts, &vec, 1.0, 1.0);
        let flat = rrf_fuse(&fts, &vec, 1.0, 100.0);
        // In sharp: gap between rank-1 and rank-10 should be much bigger
        // than in flat.
        let sharp_gap = sharp[0].1 - sharp[sharp.len() - 1].1;
        let flat_gap = flat[0].1 - flat[flat.len() - 1].1;
        assert!(sharp_gap > flat_gap);
    }
}
