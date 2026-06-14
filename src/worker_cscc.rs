// worker_cscc.rs — Cosine Similarity Cluster Consolidation (CLA-134), merge half.
//
// The nightly defrag. The encode stage is deliberately create-biased ("over is
// safe" — a re-judged unit duplicates a semantic rather than risk dropping it),
// so near-duplicate semantics accumulate: the same piece of knowledge recorded
// several times, slightly reworded, across sessions. CSCC re-clusters the WHOLE
// live semantic layer by vector proximity and asks Haiku, per cluster, whether
// the members are one subject said many ways (MERGE) or genuinely distinct
// subjects that proximity merely grouped (KEEP DISTINCT).
//
// Threshold settled by the cluster-preview calibration (the .85/.90/.93 sweep):
//   0.85 — over-merges; a size-26 blob chained through transitivity, lumping
//          distinct subjects (continuity / Hebbian / Moltbook / soul.md).
//   0.93 — too tight; misses the 4-member D1-FK-masking cluster (one fact ×4).
//   0.90 — the seam: genuine near-twins surface (FK ×4, hook-key ×2, OLED ×2,
//          music ×2, kennels ×2) with no conflation. Candidate at ≥0.90, Haiku
//          confirms keep-vs-merge, union only confirmed merges (chaining-proof).
//
// THIS FILE IS THE DRY-RUN CALIBRATOR: cluster → judge → return verdicts, NO
// writes. It exists so the merge judgment can be tuned on real output (the
// encode and orient judges each took two calibration passes) before any
// store-mutating plumbing is wired. Merge EXECUTION, the SPLIT half (re-decompose
// conflated pre-atomic semantics), the decision cache, and cron come after the
// judge reads sane here.

#![cfg(target_family = "wasm")]

use crate::hybrid;
use crate::memory::Memory;
use crate::{worker_store, worker_vectorize};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use worker::{D1Database, Env, Fetch, Headers, Method, Request, RequestInit, Result};

const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";

/// Candidate threshold settled by the cluster-preview calibration (see header).
pub const DEFAULT_THRESHOLD: f64 = 0.90;

/// Merge-judge policy version. BUMP THIS whenever `MERGE_SYSTEM_PROMPT` changes —
/// it's part of the decision cache key, so a bump invalidates every cached verdict
/// (they'd now be judged under a different prompt). Started at 1 with the cache
/// (migration 0016); the prompt's "downstream-consequences → keep_distinct" nudge
/// is folded into this v1.
const MERGE_POLICY_VER: i64 = 1;

/// Don't ask the judge about a cluster larger than this. Past it a cluster is
/// almost certainly a transitive chain (the 0.85 over-merge failure mode), not a
/// genuine near-dup set — judging "merge these 26?" is meaningless. Reported as
/// skipped-too-large rather than judged, so the cap is never silent.
const MAX_JUDGE_CLUSTER: usize = 8;

/// Hard cap on Haiku calls per calibration run.
const MAX_CLUSTERS_TO_JUDGE: usize = 40;

// ─────────────────────────────────────────────────────────────────────────────
// THE MERGE JUDGE PROMPT — the soul of this stage. Conservative by construction:
// a wrong merge silently destroys a real distinction and is hard to notice after;
// a missed merge just leaves a harmless duplicate that gets re-judged next pass.
// Tune on real output the way the encode/orient judges did.
// ─────────────────────────────────────────────────────────────────────────────
const MERGE_SYSTEM_PROMPT: &str = "You are the consolidation judge for the semantic layer of a memory system. The encode stage is deliberately biased toward CREATING new semantics ('over is safe'), so near-duplicates accumulate: the same piece of knowledge recorded several times, in slightly different words, across sessions.

You are shown a small CLUSTER of semantic memories that landed close together in embedding space. Decide ONE thing: are these the SAME subject said more than once (MERGE into a single canonical memory), or DISTINCT subjects that mere vector-proximity grouped together (KEEP DISTINCT)?

MERGE only when the members carry the SAME load-bearing claim — redundant records of one fact, one event, one understanding — so that collapsing them into the strongest single version loses nothing a future recall would want. Different wordings of the same thing: merge.

KEEP DISTINCT when the members are merely RELATED: the same topic with adjacent but different facts; a general principle sitting beside a specific instance of it; or an earlier state beside a later one (a progression — both are true, and the history matters). Proximity in embedding space is not sameness. Two true-but-different facts about one subject are still two facts. In particular, if one member records an event or an insight and another records its DOWNSTREAM consequences — what was later published, separate reactions it drew, a follow-on decision — keep them distinct: those downstream facts are content a fold would erase, not redundant rewording of the same claim.

THE BIAS IS CONSERVATIVE. When uncertain, KEEP DISTINCT. A wrong merge silently destroys a real distinction and is hard to notice afterward; a missed merge merely leaves a duplicate, which is harmless and gets re-judged next pass. Merge only when you are confident the members are genuinely the same record.

For a MERGE, name the KEEPER — the member whose wording is most complete and accurate (the members are listed strongest-first; prefer that one unless another is clearly fuller) — by its uuid. The rest fold into it.

Respond by calling the merge_decision tool.";

#[derive(Deserialize)]
struct MergeVerdict {
    /// "merge" | "keep_distinct"
    action: String,
    #[serde(default)]
    keep_id: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

/// One judged cluster, for the dry-run report.
struct JudgedCluster {
    member_ids: Vec<String>,
    member_summaries: Vec<String>,
    action: String,
    keep_id: Option<String>,
    reason: Option<String>,
}

/// Cluster the whole live semantic layer by vector proximity: pull every
/// non-superseded semantic's vector from Vectorize, compute exact pairwise
/// cosine in-Worker, union-find the pairs at/above `threshold`. Returns member
/// id-groups of size >= `min_size`, largest first.
///
/// NB: this is the same O(n²) clustering the `/admin/cluster-preview` endpoint
/// prototypes inline. When CSCC ships for real, retire the preview's copy and
/// have it call this. Kept separate for now so the working preview is untouched.
pub async fn cluster_semantics(
    env: &Env,
    db: &D1Database,
    threshold: f64,
    min_size: usize,
) -> Result<(Vec<Vec<String>>, usize)> {
    let semantics = worker_store::all_live_semantics(db).await?;

    // Pull vectors in chunks of 20 (the Vectorize binding errors above that).
    // getByIds silently drops ids it doesn't hold, so we only cluster resolved ones.
    let mut vecs: std::collections::HashMap<String, Vec<f64>> = std::collections::HashMap::new();
    for chunk in semantics.chunks(20) {
        let ids: Vec<&str> = chunk.iter().map(|s| s.id.as_str()).collect();
        for sv in worker_vectorize::get_by_ids(env, &ids).await? {
            vecs.insert(sv.id, sv.values);
        }
    }

    let nodes: Vec<&worker_store::SemanticBrief> =
        semantics.iter().filter(|s| vecs.contains_key(&s.id)).collect();
    let n = nodes.len();

    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    let mut parent: Vec<usize> = (0..n).collect();
    for i in 0..n {
        let vi = &vecs[&nodes[i].id];
        for j in (i + 1)..n {
            let vj = &vecs[&nodes[j].id];
            if hybrid::cosine_similarity(vi, vj) >= threshold {
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                if ri != rj {
                    parent[ri] = rj;
                }
            }
        }
    }

    let mut groups: std::collections::HashMap<usize, Vec<String>> = std::collections::HashMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        groups.entry(r).or_default().push(nodes[i].id.clone());
    }
    let mut clusters: Vec<Vec<String>> =
        groups.into_values().filter(|c| c.len() >= min_size).collect();
    clusters.sort_by(|a, b| b.len().cmp(&a.len()));
    // Second return = live semantics excluded because Vectorize had no vector
    // for them (get_by_ids drops unknown ids). Surfaced so a run isn't silently
    // narrower than the store — matches cluster-preview's missing_vector report.
    Ok((clusters, semantics.len() - n))
}

/// Ask Haiku whether one cluster's members are the same subject (merge) or
/// distinct (keep). Members are passed strongest-first so the keeper default
/// is the first uuid.
async fn judge_cluster(env: &Env, members: &[Memory]) -> Result<MergeVerdict> {
    let mut user = format!(
        "CLUSTER of {} semantic memories that landed within the merge threshold of each other in embedding space. Decide: same subject (merge) or distinct (keep distinct)?\n\n",
        members.len()
    );
    for (idx, m) in members.iter().enumerate() {
        user.push_str(&format!(
            "[{}] uuid: {}  (strength {:.2})\n    summary: {}\n    content: {}\n\n",
            idx + 1,
            m.id,
            m.strength,
            m.summary,
            m.content
        ));
    }
    user.push_str("Call merge_decision. Default to keep_distinct when uncertain.");

    let tool = json!({
        "name": "merge_decision",
        "description": "Decide whether this proximity cluster is one subject (merge) or distinct subjects (keep).",
        "input_schema": {
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["merge", "keep_distinct"] },
                "keep_id": { "type": "string", "description": "For merge only: full uuid of the member to keep as canonical; the rest fold into it." },
                "reason": { "type": "string", "description": "For merge: what identical claim they share. For keep: what genuinely distinguishes them (different facts, principle-vs-instance, or progression)." }
            },
            "required": ["action", "reason"]
        }
    });

    let input = call_haiku_tool(env, MERGE_SYSTEM_PROMPT, "merge_decision", tool, user, 1024).await?;
    serde_json::from_value(input)
        .map_err(|e| worker::Error::RustError(format!("parse merge_decision: {}", e)))
}

/// SHA-256 hex of a string.
fn sha_hex(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}

/// A cluster's two cache keys, both order-independent (members sorted by id):
/// the member-SET hash (identity) and the member-CONTENT hash (invalidation).
/// A member added/removed changes the set hash; a member reframed changes the
/// content hash. Sorted by id — NOT strength — so decay can't shift the hash.
fn cluster_and_content_hash(members: &[Memory]) -> (String, String) {
    let mut sorted: Vec<&Memory> = members.iter().collect();
    sorted.sort_by(|a, b| a.id.cmp(&b.id));
    let ids = sorted.iter().map(|m| m.id.as_str()).collect::<Vec<_>>().join(",");
    let contents = sorted
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\u{1f}");
    (sha_hex(&ids), sha_hex(&contents))
}

/// This cluster's merge verdict, from the cache or a fresh Haiku judge. Cache hit
/// requires member-set hash AND content hash AND policy version all match — so a
/// reframed member or a tuned prompt re-judges, while an unchanged cluster reuses
/// the SAME verdict (keeper included). That last property is what makes dry-run →
/// commit deterministic: the dry-run caches the verdict, the commit reads it back.
/// `use_cache = false` forces a fresh judge (still writes the result). Every fresh
/// judge upserts, so a dry-run populates the cache the commit consumes.
/// Returns (verdict, was_cache_hit).
async fn decide_cluster(
    env: &Env,
    db: &D1Database,
    members: &[Memory],
    use_cache: bool,
) -> Result<(MergeVerdict, bool)> {
    let (cluster_hash, content_hash) = cluster_and_content_hash(members);
    if use_cache {
        if let Some(c) = worker_store::fetch_cscc_decision(db, &cluster_hash).await? {
            if c.content_hash == content_hash && c.policy_ver == MERGE_POLICY_VER {
                return Ok((
                    MergeVerdict {
                        action: c.action,
                        keep_id: c.keeper_id,
                        reason: Some("(cached)".to_string()),
                    },
                    true,
                ));
            }
        }
    }
    let verdict = judge_cluster(env, members).await?;
    worker_store::upsert_cscc_decision(
        db,
        &cluster_hash,
        &content_hash,
        MERGE_POLICY_VER,
        &verdict.action,
        verdict.keep_id.as_deref(),
    )
    .await?;
    Ok((verdict, false))
}

/// Dry-run calibration: cluster the live semantic layer at `threshold`, judge
/// each cluster (merge / keep-distinct), return the verdicts. NO writes.
pub async fn calibrate(
    env: &Env,
    db: &D1Database,
    threshold: f64,
    min_size: usize,
    use_cache: bool,
) -> Result<Value> {
    let (clusters, missing_vector) = cluster_semantics(env, db, threshold, min_size).await?;
    let clusters_total = clusters.len();

    let mut judged: Vec<JudgedCluster> = Vec::new();
    let mut skipped_too_large = 0usize;
    let mut judged_count = 0usize;
    let mut cache_hits = 0usize;

    for cluster_ids in &clusters {
        if cluster_ids.len() > MAX_JUDGE_CLUSTER {
            skipped_too_large += 1;
            continue;
        }
        if judged_count >= MAX_CLUSTERS_TO_JUDGE {
            break;
        }
        judged_count += 1;

        let id_refs: Vec<&str> = cluster_ids.iter().map(String::as_str).collect();
        let mut members = worker_store::get_many(db, &id_refs).await?;
        // Strongest first — the keeper default.
        members.sort_by(|a, b| {
            b.strength
                .partial_cmp(&a.strength)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if members.len() < 2 {
            continue;
        }

        let (verdict, cache_hit) = decide_cluster(env, db, &members, use_cache).await?;
        if cache_hit {
            cache_hits += 1;
        }
        judged.push(JudgedCluster {
            member_ids: members.iter().map(|m| m.id.clone()).collect(),
            member_summaries: members.iter().map(|m| m.summary.clone()).collect(),
            action: verdict.action,
            keep_id: verdict.keep_id,
            reason: verdict.reason,
        });
    }

    let merges = judged.iter().filter(|j| j.action == "merge").count();
    let keeps = judged.len() - merges;

    let detail: Vec<Value> = judged
        .iter()
        .map(|j| {
            json!({
                "action": j.action,
                "keep": j.keep_id.as_deref().map(|s| &s[..s.len().min(8)]),
                "members": j.member_ids.iter().zip(j.member_summaries.iter())
                    .map(|(id, s)| json!({ "id": &id[..id.len().min(8)], "summary": s }))
                    .collect::<Vec<_>>(),
                "reason": j.reason,
            })
        })
        .collect();

    Ok(json!({
        "threshold": threshold,
        "min_size": min_size,
        "clusters_total": clusters_total,
        "judged": judged.len(),
        "skipped_too_large": skipped_too_large,
        "skipped_budget": clusters_total.saturating_sub(judged_count).saturating_sub(skipped_too_large),
        "missing_vector": missing_vector,
        "cache_hits": cache_hits,
        "merge": merges,
        "keep_distinct": keeps,
        "dry_run": true,
        "detail": detail,
    }))
}

/// One planned (or executed) fold, for the report.
struct FoldPlan {
    keeper_id: String,
    keeper_summary: String,
    /// (id, summary) of each member folded into the keeper.
    folded: Vec<(String, String)>,
    /// Lineage edges that would repoint (dry-run) / did repoint (commit), summed
    /// over the folded members.
    edges_repointed: u64,
    /// True if the judge's keep_id wasn't a real cluster member and we fell back
    /// to the strongest member — a safety net against a hallucinated keeper.
    keeper_fallback: bool,
    committed: bool,
}

/// Execute (or dry-run) the confirmed merges over the live semantic layer.
/// Clusters at `threshold`, judges each, and for every MERGE verdict folds the
/// non-keeper members into the keeper: repoint their lineage edges onto the
/// keeper, then `mark_superseded` them (kept as history, `superseded_by` =
/// keeper). KEEP_DISTINCT clusters are reported, untouched.
///
/// `commit = false` (the caller's default) PLANS only — NO writes — and reports
/// exactly what a commit would do. Idempotent under repeat commits: a folded
/// member is superseded, so it drops out of `all_live_semantics` and the cluster
/// never re-forms; the keeper remains. (CLA-134, merge half.)
pub async fn execute_merges(
    env: &Env,
    db: &D1Database,
    threshold: f64,
    min_size: usize,
    commit: bool,
    use_cache: bool,
) -> Result<Value> {
    let (clusters, missing_vector) = cluster_semantics(env, db, threshold, min_size).await?;
    let clusters_total = clusters.len();

    let mut plans: Vec<FoldPlan> = Vec::new();
    let mut keeps = 0usize;
    let mut skipped_too_large = 0usize;
    let mut judged = 0usize;
    let mut cache_hits = 0usize;

    for cluster_ids in &clusters {
        if cluster_ids.len() > MAX_JUDGE_CLUSTER {
            skipped_too_large += 1;
            continue;
        }
        if judged >= MAX_CLUSTERS_TO_JUDGE {
            break;
        }
        judged += 1;

        let id_refs: Vec<&str> = cluster_ids.iter().map(String::as_str).collect();
        let mut members = worker_store::get_many(db, &id_refs).await?;
        members.sort_by(|a, b| {
            b.strength
                .partial_cmp(&a.strength)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if members.len() < 2 {
            continue;
        }

        let (verdict, cache_hit) = decide_cluster(env, db, &members, use_cache).await?;
        if cache_hit {
            cache_hits += 1;
        }
        if verdict.action != "merge" {
            keeps += 1;
            continue;
        }

        // Resolve the keeper. The judge returns a full uuid; VALIDATE it is an
        // actual cluster member — never supersede a whole cluster in favour of a
        // hallucinated id. Fall back to the strongest member (members[0]).
        let member_ids: std::collections::HashSet<&str> =
            members.iter().map(|m| m.id.as_str()).collect();
        let (keeper_id, keeper_fallback) = match verdict.keep_id.as_deref() {
            Some(k) if member_ids.contains(k) => (k.to_string(), false),
            _ => (members[0].id.clone(), true),
        };
        let keeper_summary = members
            .iter()
            .find(|m| m.id == keeper_id)
            .map(|m| m.summary.clone())
            .unwrap_or_default();

        let folded: Vec<(String, String)> = members
            .iter()
            .filter(|m| m.id != keeper_id)
            .map(|m| (m.id.clone(), m.summary.clone()))
            .collect();

        let mut edges_repointed = 0u64;
        for (fid, _) in &folded {
            edges_repointed += worker_store::lineage_edge_count(db, fid).await?;
        }

        if commit {
            for (fid, _) in &folded {
                // One atomic batch per fold: the edge repoint AND the supersede
                // land together or not at all — never a live semantic stranded
                // with no lineage if a write fails mid-fold (CLA-134 review).
                let mut stmts = worker_store::repoint_lineage_edge_stmts(db, fid, &keeper_id)?;
                stmts.push(worker_store::mark_superseded_stmt(db, fid, &keeper_id)?);
                db.batch(stmts).await?;
            }
        }

        plans.push(FoldPlan {
            keeper_id,
            keeper_summary,
            folded,
            edges_repointed,
            keeper_fallback,
            committed: commit,
        });
    }

    // Sync Vectorize to the live set: a superseded semantic's vector is dead
    // (get_many filters superseded on hydration, so it can't surface) but still
    // eats retrieval slots. Purge on commit — idempotent, best-effort, and cleans
    // this run's folds plus any historical supersession (encode corrections).
    let mut vectors_purged = 0usize;
    if commit {
        let dead = worker_store::superseded_semantic_ids(db).await?;
        for chunk in dead.chunks(20) {
            let refs: Vec<&str> = chunk.iter().map(String::as_str).collect();
            let _ = worker_vectorize::delete_ids(env, &refs).await;
        }
        vectors_purged = dead.len();
    }

    let semantics_folded: usize = plans.iter().map(|p| p.folded.len()).sum();
    let detail: Vec<Value> = plans
        .iter()
        .map(|p| {
            json!({
                "keeper": &p.keeper_id[..p.keeper_id.len().min(8)],
                "keeper_summary": p.keeper_summary,
                "keeper_fallback": p.keeper_fallback,
                "folds": p.folded.iter()
                    .map(|(id, s)| json!({ "id": &id[..id.len().min(8)], "summary": s }))
                    .collect::<Vec<_>>(),
                "edges_repointed": p.edges_repointed,
                "committed": p.committed,
            })
        })
        .collect();

    Ok(json!({
        "threshold": threshold,
        "min_size": min_size,
        "commit": commit,
        "clusters_total": clusters_total,
        "judged": judged,
        "skipped_too_large": skipped_too_large,
        "skipped_budget": clusters_total.saturating_sub(judged).saturating_sub(skipped_too_large),
        "missing_vector": missing_vector,
        "cache_hits": cache_hits,
        "merges": plans.len(),
        "keep_distinct": keeps,
        "semantics_folded": semantics_folded,
        "vectors_purged": vectors_purged,
        "detail": detail,
    }))
}

/// One forced-tool Haiku call → the validated tool `input` object. Token-type
/// aware (sk-ant-api → x-api-key; sk-ant-oat → Bearer); the static system prompt
/// is cached. Mirrors the helper in worker_orient_distill.
async fn call_haiku_tool(
    env: &Env,
    system_prompt: &str,
    tool_name: &str,
    tool_definition: Value,
    user_message: String,
    max_tokens: u32,
) -> Result<Value> {
    let oauth_token = env.secret("CLAUDE_CODE_OAUTH_TOKEN")?.to_string();
    let body = json!({
        "model": HAIKU_MODEL,
        "max_tokens": max_tokens,
        "system": [{
            "type": "text",
            "text": system_prompt,
            "cache_control": { "type": "ephemeral" }
        }],
        "tools": [tool_definition],
        "tool_choice": { "type": "tool", "name": tool_name },
        "messages": [{ "role": "user", "content": user_message }]
    });

    let headers = Headers::new();
    if oauth_token.starts_with("sk-ant-api") {
        headers.set("x-api-key", &oauth_token)?;
    } else {
        headers.set("Authorization", &format!("Bearer {}", oauth_token))?;
    }
    headers.set("anthropic-version", "2023-06-01")?;
    headers.set("content-type", "application/json")?;

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(body.to_string().into()));

    let req = Request::new_with_init("https://api.anthropic.com/v1/messages", &init)?;
    let mut resp = Fetch::Request(req).send().await?;

    if resp.status_code() >= 400 {
        let err_text = resp.text().await.unwrap_or_else(|_| "no body".to_string());
        return Err(worker::Error::RustError(format!(
            "Anthropic API {} : {}",
            resp.status_code(),
            err_text
        )));
    }

    let body_text = resp.text().await?;
    let response_json: Value = serde_json::from_str(&body_text)
        .map_err(|e| worker::Error::RustError(format!("parse response: {}", e)))?;

    let content = response_json
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or_else(|| worker::Error::RustError("no content array".to_string()))?;
    let tool_use = content
        .iter()
        .find(|item| item.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
        .ok_or_else(|| worker::Error::RustError("no tool_use block".to_string()))?;
    let input = tool_use
        .get("input")
        .ok_or_else(|| worker::Error::RustError("tool_use missing input".to_string()))?;
    Ok(input.clone())
}
