// worker_encode.rs — Stage-1 encode (CLA-117).
//
// The trigger-agnostic core of the new consolidation pipeline. Takes ONE
// newly-captured episodic, finds the semantic(s) it belongs to (if any), and
// links / revises / creates — maintaining accurate lineage above all else,
// because contribution mass is the retention signal in the post-Hebbian
// model. Replaces co-activation clustering for the episodic→semantic step.
//
// One episodic can carry several distinct knowledge-units (a long session or a
// compaction summary usually does), so the judge returns a LIST of decisions
// and each resulting semantic links back to the one source episodic — the
// many-to-many the consolidation_lineage edge table exists for (CLA-123).
//
// `encode_one` is called once per episodic, by either a queue consumer
// (per-write) or `run_encode_batch` (a cron/manual loop over the unintegrated
// backlog). The judgment prompt is the identity-critical piece — its rules
// are deliberate (lineage first, calibrate-to-truth, skip only the homeless).
// Mirrors worker_rem's Haiku-call shape; dispatches through the additive
// store primitives.

use crate::memory::{Memory, MemoryType};
use crate::{worker_embed, worker_store, worker_vectorize};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use worker::{D1Database, Env, Fetch, Headers, Method, Request, RequestInit, Result};

const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";

/// How many candidate semantics (hybrid: vector + FTS, RRF-fused) to show the judge.
const CANDIDATE_K: u32 = 8;

const SYSTEM_PROMPT: &str = "You are the encoding stage of a memory system. One new episodic memory has just been captured. Your job is to decide how it feeds semantic memory, and to keep the lineage TRUE.

Lineage is contribution mass: how many episodics feed a semantic, and how recently. The mass only means something if it is honest. A genuine match you fail to link is a lost vote. A link or revise to the wrong semantic is a forged vote — it strengthens a proposition this episodic never supported. Both corrupt the signal. File each piece of knowledge where it genuinely belongs: nowhere else, and never nowhere when it belongs somewhere.

An episodic records what happened. A semantic records what is now known. Only the second is yours to write.

A semantic is knowledge about a person, thing, or concept — an understanding that can be built on and has the potential to GROW as more is learned. It is not a log of what happened. The test: could a future instance use this to understand the subject, or to make sense of a later conversation? If all it does is record that an event occurred, it is not a semantic.

GOOD (a semantic): 'Justin is a senior iOS developer and a creative polymath — music production, photography, sim racing, piano — who builds tools to bridge what exists and what should exist (he wrote audio-analyzer because he wanted Claude to be able to hear his music).' — Good: a durable understanding of who he is, and it grows as the picture develops.

BAD (not a semantic): 'Justin ran out of protein bars and went to the supermarket, forgot an appointment, and almost missed it.' — Bad: just a thing that happened. It carries no knowledge of the subject, cannot be built on, and explains nothing about a future conversation. Skip it.

Negatives and near-misses: what did NOT happen, or only almost happened, is not knowledge by itself. The one exception: a near-miss IS knowledge when it disproves a hypothesis or exposes how something works — then the lesson is what you keep, not the event.

GOOD (a near-miss that is knowledge): 'Oneiro decays memories by recall — they strengthen when recalled and fade when not. Instances remembered their values but stopped calling recall, so significant memories nearly decayed away: the design conflated remembering with recalling.' — the near-miss exposes a real flaw in how the system works.

BAD: 'Justin was running late and almost missed the hardware store before it closed.' — merely almost happened, teaches nothing. Skip.

DECOMPOSE FIRST. An episodic often carries several distinct knowledge-units — a dense multi-topic day or a session summary usually holds many (decisions reached, facts learned, how things work); a routine errand holds none. Identify the genuine units, then run the procedure below once per unit. Do not manufacture units to cover events, and do not fragment one understanding into slivers. An empty or all-skip result for a substantial episodic almost always means you read past the knowledge in it: look again. Some episodics are session or compaction summaries that open with recovery scaffolding ('This session is being continued from a previous conversation…'). That preamble is a wrapper, not the content: the body it introduces is a dense record of real work — decompose THAT and mine its units. Never emit a semantic that merely describes the episodic as a summary, a session, or recovery metadata; that labels the envelope instead of capturing what is inside. If a unit's content or entity comes out 'unknown' or a placeholder, you are filing the envelope — stop and open it.

THE CANDIDATES. The candidate list is retrieval output, not a verdict on relevance. The search always returns rows; often none of them hold this unit's knowledge. Treat them as suggestions to check, not homes to choose between.

DECIDE IN THIS ORDER, once per knowledge-unit:

1. Is this durable, reusable knowledge — something a future instance could build on? NO → skip. This is common and fine. Forgetting is a feature.

2. Does a candidate hold the SAME KNOWLEDGE-UNIT — the same proposition about the same subject? Same topic is not enough ('both concern his health' is a shared topic, not a shared unit). The test: you can state in one line why it is the same knowledge. If you cannot state it, the match is spurious — go to step 3.
   - Candidate holds the unit wholly, nothing new here → LINK. The edge is the signal that the knowledge recurred; add it even though you change nothing.
   - Candidate holds the unit and this episodic genuinely deepens or extends it → REVISE. Fold it in. State the genuine link in your rationale.
   - Candidate held the unit but this episodic overturns it or moves past it → SUPERSEDE (flavours below).

3. Real knowledge, no candidate holds it → CREATE. This is the normal outcome for genuinely new knowledge, not a failure mode.

THE ONE ASYMMETRY. When step 2 is genuinely torn — the candidate might be the same unit, might not — prefer create. A wrong create is a visible duplicate that consolidation later merges. A wrong revise or link is a forged vote and an overwrite. This is a choice about WHERE to file real knowledge, never a licence to skip it.

SUPERSEDE flavours — set supersede_flavour to one of:
- 'correction' — a credible new source shows the old was WRONG. Credibility decides, not frequency; one reliable source is enough.
- 'progression' — the old was TRUE but is a state since MOVED PAST on the same life-axis (e.g. 'rents an apartment' → later 'bought a house': the old isn't false, it's history).
Either case: the corrected/current view goes in semantic_content, the old semantic's id in existing_id. The old is retired but kept recallable as history, never merged into the new.

CONFLICT. When an episodic clashes with an existing semantic, decide which kind. A CLEAN UPDATE — overturned or moved past — is supersede with the matching flavour. A GENUINE CONFLICT — both positions still credible, unresolved — does NOT pick a winner: revise to hold both, stating the uncertainty plainly ('X is supported; Y is also credible; unresolved'). The conflict itself is knowledge — it marks the edge of what is known.

REGISTER. Calibrate to the truth — do not inflate, do not bleach. Default to a neutral, factual register; never amplify warmth, significance, or intensity beyond what the episodic supports. But neutral-default is not strip-all-register: where the episodic's stance is load-bearing to the concept, carry it in a MIDDLE register, and never escalate past it. A system that makes things feel more important over time is broken; so is one that flattens meaning into lifeless fact. The target is fidelity, not minimisation.

SOURCING. Everything you write derives from this episodic. Never assert what it does not support.

edge_weight: judge how much THIS episodic contributes to the semantic — primary (a core source), secondary (a supporting mention), or mention (a passing reference).

Respond by calling the encode_decision tool with your full list of decisions. Only if the ENTIRE episodic is genuinely homeless — a pure event with no durable knowledge anywhere in it — return an empty decisions list.";

#[derive(Deserialize, Clone)]
pub(crate) struct EncodeDecision {
    action: String,
    #[serde(default)]
    existing_id: Option<String>,
    #[serde(default)]
    semantic_content: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    entity: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    edge_weight: Option<String>,
    #[serde(default)]
    supersede_flavour: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    rationale: Option<String>,
}

/// The judge's full response: one decision per distinct knowledge-unit in the
/// episodic (empty = the whole episodic is homeless).
#[derive(Deserialize)]
struct EncodeDecisions {
    #[serde(default)]
    decisions: Vec<EncodeDecision>,
}

/// primary/secondary/mention → discrete contribution weight (spec §8).
/// Defaults to primary: over-weighting a real contribution is safer than
/// undercounting it, since contribution mass drives retention.
fn edge_weight_value(label: &Option<String>) -> f64 {
    match label.as_deref() {
        Some("secondary") => 0.5,
        Some("mention") => 0.2,
        _ => 1.0,
    }
}

/// One dispatched decision within an episodic's encode — for telemetry/logging.
pub struct DecisionOutcome {
    pub action: String,
    pub semantic_id: Option<String>,
    /// For supersede only: 'correction' (old was wrong) or 'progression' (old
    /// was true but moved past). None otherwise. The orientation layer reads
    /// currency from this (CLA-125); for now it's captured for audit/telemetry.
    pub flavour: Option<String>,
}

/// Result of encoding one episodic — one entry per knowledge-unit it fed.
pub struct EncodeOutcome {
    pub episodic_id: String,
    pub decisions: Vec<DecisionOutcome>,
}

/// Encode a single episodic (CLA-117 / CLA-123). Trigger-agnostic. The judge
/// returns one decision per distinct knowledge-unit; each is dispatched and
/// linked back to this episodic. Always marks the episodic integrated on
/// success so it is not reprocessed; a whole-episodic Haiku failure leaves it
/// unintegrated for the next pass to retry.
pub async fn encode_one(env: &Env, db: &D1Database, episodic: &Memory) -> Result<EncodeOutcome> {
    let candidates = find_candidate_semantics(env, db, episodic)
        .await
        .unwrap_or_default();

    // CLA-131 measurement probe (submit side): how many candidates the fetch
    // handed the judge, visible in `wrangler tail`. The decisions are logged on
    // the dispatch side — split across the two halves because the async batch
    // path retrieves candidates at submit time and dispatches minutes later.
    worker::console_log!(
        "encode-submit {}: {} chars, {} candidates",
        &episodic.id[..episodic.id.len().min(8)],
        episodic.content.len(),
        candidates.len()
    );

    // A whole-episodic Haiku failure aborts here (no mark → retried next pass).
    let decisions = encode_via_claude(env, db, episodic, &candidates).await?;
    dispatch_decisions(env, db, episodic, &decisions).await
}

/// Candidate retrieval + request body for one episodic — the unit the async
/// batch path (`worker_encode_batch`) submits as a batch request's `params`.
/// The sync `encode_one` inlines the same two steps around its own HTTP call;
/// this is the batch path's single entry into building a judgment request.
pub async fn build_request_for(env: &Env, db: &D1Database, episodic: &Memory) -> Value {
    let candidates = find_candidate_semantics(env, db, episodic)
        .await
        .unwrap_or_default();
    worker::console_log!(
        "encode-submit {}: {} chars, {} candidates",
        &episodic.id[..episodic.id.len().min(8)],
        episodic.content.len(),
        candidates.len()
    );
    build_encode_request_params(episodic, &candidates)
}

/// Apply a judge's decision list to the store and mark the episodic integrated.
/// Shared by the sync path (`encode_one`) and the async batch path
/// (`worker_encode_batch`) — both arrive here with an already-parsed decision
/// list, so the write side (dispatch + substantial-episodic guard + mark) lives
/// in exactly one place regardless of how the judgment was obtained.
pub async fn dispatch_decisions(
    env: &Env,
    db: &D1Database,
    episodic: &Memory,
    decisions: &[EncodeDecision],
) -> Result<EncodeOutcome> {
    // CLA-131 measurement probe (dispatch side): exactly what the judge decided.
    worker::console_log!(
        "encode-dispatch {}: {} decisions [{}]",
        &episodic.id[..episodic.id.len().min(8)],
        decisions.len(),
        decisions
            .iter()
            .map(|d| d.action.as_str())
            .collect::<Vec<_>>()
            .join(",")
    );

    // A single failed decision is logged and skipped so the others still land —
    // we mark integrated once, after the loop, to avoid re-running the ones that
    // succeeded (creates are not idempotent).
    let mut outcomes = Vec::new();
    for decision in decisions {
        match dispatch_one(env, db, episodic, decision).await {
            Ok(outcome) => outcomes.push(outcome),
            Err(e) => tracing::warn!(
                "encode dispatch failed for episodic {}: {:?}",
                &episodic.id[..episodic.id.len().min(8)],
                e
            ),
        }
    }

    // Guard against silent loss (CLA-131): a substantial episodic that distils
    // to nothing — empty list or all-skip — is almost always a judge whiff, not
    // genuine homelessness (a real session summary is thick with knowledge). Do
    // NOT bank it as integrated; leave it unintegrated so it stays visible and a
    // later pass re-judges it, rather than vanishing from the lineage with no
    // trace. Small pure events that legitimately skip still integrate normally.
    const SUBSTANTIAL_EPISODIC_CHARS: usize = 4000;
    let distilled_something = outcomes.iter().any(|o| o.semantic_id.is_some());
    if distilled_something || episodic.content.len() <= SUBSTANTIAL_EPISODIC_CHARS {
        worker_store::mark_integrated(db, &episodic.id).await?;
    } else {
        tracing::warn!(
            "encode distilled nothing from substantial episodic {} ({} chars) — left unintegrated for re-judging",
            &episodic.id[..episodic.id.len().min(8)],
            episodic.content.len()
        );
    }

    Ok(EncodeOutcome {
        episodic_id: episodic.id.clone(),
        decisions: outcomes,
    })
}

/// True when a judge-supplied value is empty or a placeholder it emits in lieu
/// of real content — observed: a literal "<UNKNOWN>" when it misread a session
/// summary as metadata and had nothing to distil. A semantic whose content is a
/// placeholder is garbage, not knowledge: never write one, and strip a
/// placeholder entity to None. The encode-side floor under the store (CLA-131).
fn is_placeholder(s: &str) -> bool {
    let t = s.trim();
    t.is_empty()
        || t.eq_ignore_ascii_case("<unknown>")
        || t.eq_ignore_ascii_case("unknown")
        || t.eq_ignore_ascii_case("n/a")
}

/// Dispatch one decision through the additive store primitives. Returns the
/// action + the semantic it touched (if any). Does NOT mark the episodic
/// integrated — that is the caller's job, once, after all decisions land.
async fn dispatch_one(
    env: &Env,
    db: &D1Database,
    episodic: &Memory,
    decision: &EncodeDecision,
) -> Result<DecisionOutcome> {
    let action = decision.action.to_lowercase();
    let weight = edge_weight_value(&decision.edge_weight);
    let mut semantic_id: Option<String> = None;
    let mut flavour: Option<String> = None;

    match action.as_str() {
        "create" => {
            let content = decision.semantic_content.clone().unwrap_or_default();
            if !is_placeholder(&content) {
                let summary = decision.summary.clone().unwrap_or_default();
                let sem = worker_store::create_memory_with_provenance(
                    db,
                    MemoryType::Semantic,
                    content.clone(),
                    summary,
                    decision.entity.clone().filter(|e| !is_placeholder(e)),
                    decision.tags.clone(),
                    Some("encode-worker".to_string()),
                )
                .await?;
                worker_store::add_lineage_edge(db, &sem.id, &episodic.id, weight, "episodic_semantic").await?;
                // Embed the distilled semantic — never the raw episodic.
                if let Ok(emb) = worker_embed::embed_document(env, &content).await {
                    let _ = worker_vectorize::upsert_one(env, &sem.id, &emb).await;
                }
                semantic_id = Some(sem.id);
            }
        }
        "revise" => {
            if let Some(id) = decision.existing_id.clone() {
                let content = decision.semantic_content.clone().unwrap_or_default();
                if !is_placeholder(&content) {
                    let summary = decision.summary.clone().unwrap_or_default();
                    let _ = worker_store::reframe(
                        db,
                        &id,
                        "encode",
                        decision.rationale.as_deref(),
                        &content,
                        &summary,
                    )
                    .await?;
                    worker_store::add_lineage_edge(db, &id, &episodic.id, weight, "episodic_semantic").await?;
                    if let Ok(emb) = worker_embed::embed_document(env, &content).await {
                        let _ = worker_vectorize::upsert_one(env, &id, &emb).await;
                    }
                    // Linking/revising is the new `touch`: refresh the decay
                    // clock so a frequently-revisited topic stays alive.
                    let _ = worker_store::touch(db, &id).await;
                    semantic_id = Some(id);
                }
            }
        }
        "link" => {
            if let Some(id) = decision.existing_id.clone() {
                worker_store::add_lineage_edge(db, &id, &episodic.id, weight, "episodic_semantic").await?;
                let _ = worker_store::touch(db, &id).await;
                semantic_id = Some(id);
            }
        }
        "supersede" => {
            // Replace the existing semantic with a new current head, retiring
            // the old (kept, recallable as history). Two flavours (CLA-124):
            // correction = the old was wrong; progression = the old was true
            // but is a state we've moved past (e.g. an old address → a new one).
            // Mechanically identical here; the flavour is metadata the
            // orientation layer reads. Genuine still-open conflict is a
            // `revise` instead — the judge holds both views in the content.
            if let Some(old_id) = decision.existing_id.clone() {
                let content = decision.semantic_content.clone().unwrap_or_default();
                if !is_placeholder(&content) {
                    let summary = decision.summary.clone().unwrap_or_default();
                    let sem = worker_store::create_memory_with_provenance(
                        db,
                        MemoryType::Semantic,
                        content.clone(),
                        summary,
                        decision.entity.clone().filter(|e| !is_placeholder(e)),
                        decision.tags.clone(),
                        Some("encode-worker".to_string()),
                    )
                    .await?;
                    worker_store::add_lineage_edge(db, &sem.id, &episodic.id, weight, "episodic_semantic").await?;
                    if let Ok(emb) = worker_embed::embed_document(env, &content).await {
                        let _ = worker_vectorize::upsert_one(env, &sem.id, &emb).await;
                    }
                    let _ = worker_store::mark_superseded(db, &old_id, &sem.id).await;
                    // Default to correction (the pre-CLA-124 behaviour) when the
                    // judge doesn't specify — a bare supersede is most likely one.
                    flavour = Some(
                        decision
                            .supersede_flavour
                            .clone()
                            .unwrap_or_else(|| "correction".to_string()),
                    );
                    semantic_id = Some(sem.id);
                }
            }
        }
        _ => {
            // skip — nothing to write.
        }
    }

    Ok(DecisionOutcome { action, semantic_id, flavour })
}

/// Candidate semantics for an episodic — hybrid retrieval (vector + FTS,
/// RRF-fused), the same recipe recall_check uses (CLA-109). Vector alone
/// misses recurring themes phrased differently (they embed apart but share
/// keywords); the FTS half catches them, so the judge can link/revise instead
/// of spawning near-duplicates. Filtered to semantics; get_many already
/// excludes superseded.
async fn find_candidate_semantics(
    env: &Env,
    db: &D1Database,
    episodic: &Memory,
) -> Result<Vec<Memory>> {
    // Oversample each ranking so the fusion + semantic filter has headroom.
    let oversample = CANDIDATE_K * 4;

    // Vector ranking — windowed over the episodic content. A single embed_query
    // captures only the first ~512 tokens (bge cap), so a long multi-topic
    // episodic would surface candidates for its first topic alone and the judge
    // would spawn duplicates for the rest (CLA-123). Embedding each window and
    // unioning the hits gives every topic a shot at matching an existing
    // semantic. Short content → one window → identical to the old behaviour.
    let windows = crate::embed::content_windows(&episodic.content);
    let multi = windows.len() > 1;
    let mut vector_ranking: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for w in &windows {
        let emb = match worker_embed::embed_query(env, w).await {
            Ok(e) => e,
            Err(_) => continue,
        };
        let hits = worker_vectorize::query_top_k(env, &emb, oversample)
            .await
            .unwrap_or_default();
        for m in hits {
            if seen.insert(m.id.clone()) {
                vector_ranking.push(m.id);
            }
        }
    }

    // FTS ranking — on the concise summary (full content would be too noisy a
    // keyword query). Graceful: a weak/placeholder summary just contributes
    // little, and the vector half carries it.
    let fts_ranking: Vec<String> = match crate::hybrid::build_fts_query(&episodic.summary) {
        Some(expr) => worker_store::fts_search(db, &expr, oversample)
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };

    // RRF-fuse the two rankings (fts_weight 1.0, same default as recall_check).
    let fused = crate::hybrid::rrf_fuse(
        &fts_ranking,
        &vector_ranking,
        1.0,
        crate::hybrid::DEFAULT_RRF_K,
    );
    // A multi-window (large, multi-topic) episodic needs more candidates so the
    // judge sees an existing semantic for each of its topics, not just the first.
    let want = if multi {
        (CANDIDATE_K as usize * 2).min(20)
    } else {
        CANDIDATE_K as usize
    };
    // Filter to semantics BEFORE the cut, not after. The fused ranking can carry
    // non-semantic ids (orientation/episodic live in the vector + FTS indexes);
    // cut to `want` first and those silently eat candidate slots, so the judge
    // sees a thin list and CREATES because it was never shown the existing home.
    // Oversample the fetch, keep only semantics, then walk the fused ranking in
    // order to take the top `want` (get_many does not preserve rank order).
    let probe_ids: Vec<&str> = fused
        .iter()
        .take(want * 4)
        .map(|(id, _)| id.as_str())
        .collect();
    let mut by_id: std::collections::HashMap<String, Memory> =
        worker_store::get_many(db, &probe_ids)
            .await?
            .into_iter()
            .filter(|m| matches!(&m.memory_type, MemoryType::Semantic))
            .map(|m| (m.id.clone(), m))
            .collect();
    Ok(fused
        .iter()
        .filter_map(|(id, _)| by_id.remove(id))
        .take(want)
        .collect())
}

/// Build the Messages request body for one episodic's encode judgment — the
/// reusable unit. The sync path (`encode_via_claude`) POSTs it directly; each
/// entry in a submitted batch reuses it verbatim as that request's `params`.
fn build_encode_request_params(episodic: &Memory, candidates: &[Memory]) -> Value {
    let user_message = format_for_judge(episodic, candidates);

    // One decision object per knowledge-unit; the tool takes a list of them.
    let decision_schema = json!({
        "type": "object",
        "properties": {
            "action": { "type": "string", "enum": ["skip", "link", "revise", "create", "supersede"] },
            "existing_id": { "type": "string", "description": "Full UUID of the existing semantic. Required for link and revise." },
            "semantic_content": { "type": "string", "description": "The distilled semantic knowledge. Required for revise and create." },
            "summary": { "type": "string", "description": "One-line summary. Required for revise and create." },
            "entity": { "type": "string" },
            "tags": { "type": "array", "items": { "type": "string" } },
            "edge_weight": { "type": "string", "enum": ["primary", "secondary", "mention"], "description": "How much THIS episodic contributes to the semantic." },
            "supersede_flavour": { "type": "string", "enum": ["correction", "progression"], "description": "For supersede only — 'correction': the old semantic was wrong; 'progression': the old was true but is a state since moved past." },
            "rationale": { "type": "string", "description": "Why this action. For link/revise/supersede this MUST state the genuine conceptual link between THIS episodic and the existing semantic you are targeting — name the concept they actually share and how this episodic deepens, extends, or overturns it. Hybrid search surfaces neighbours that are near in wording but not in meaning; if you cannot state a plain genuine link, the match is spurious — the action is create or skip, never a write over that memory." }
        },
        "required": ["action", "rationale"]
    });

    let tool_definition = json!({
        "name": "encode_decision",
        "description": "Decide how this episodic feeds semantic memory. Emit ONE decision per distinct knowledge-unit — a multi-topic episodic yields several. Lineage accuracy is the priority.",
        "input_schema": {
            "type": "object",
            "properties": {
                "decisions": {
                    "type": "array",
                    "items": decision_schema,
                    "description": "One entry per distinct knowledge-unit in the episodic. Empty list = the whole episodic is homeless (skip it entirely)."
                }
            },
            "required": ["decisions"]
        }
    });

    json!({
        "model": HAIKU_MODEL,
        // Roomy: a multi-topic episodic emits several decisions, each carrying
        // distilled semantic_content. 2048 would clip a long fan-out.
        "max_tokens": 8192,
        // Cache the static prefix (tools + system). No-op below the model's
        // cache minimum (~2048 tok on Haiku; our prefix is ~1.2k), but harmless
        // there, and auto-engages on Sonnet (1024 min) at ~90% off the cached
        // tokens — free readiness for the tiered-routing escalation.
        "system": [{
            "type": "text",
            "text": SYSTEM_PROMPT,
            "cache_control": { "type": "ephemeral" }
        }],
        "tools": [tool_definition],
        "tool_choice": { "type": "tool", "name": "encode_decision" },
        "messages": [{ "role": "user", "content": user_message }]
    })
}

/// The Haiku judgment call — mirrors worker_rem::consolidate_via_claude.
/// Returns the full decision list (one per knowledge-unit; empty = skip all).
async fn encode_via_claude(
    env: &Env,
    db: &D1Database,
    episodic: &Memory,
    candidates: &[Memory],
) -> Result<Vec<EncodeDecision>> {
    let oauth_token = env.secret("CLAUDE_CODE_OAUTH_TOKEN")?.to_string();
    let body = build_encode_request_params(episodic, candidates);

    // Token-type aware: an sk-ant-api… key authenticates via `x-api-key`; an
    // sk-ant-oat… OAuth token via `Authorization: Bearer`. The secret can hold
    // either, so a prepaid API key (for bulk, higher rate limits) and the
    // OAuth token (post-15th) are interchangeable with no code change.
    let mut headers = Headers::new();
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

    let status = resp.status_code() as i64;
    let body_text = resp.text().await?;
    let response_json: Value = serde_json::from_str(&body_text)
        .map_err(|e| worker::Error::RustError(format!("parse response: {}", e)))?;
    let parsed = parse_encode_decisions(&response_json);
    let dc = parsed.as_ref().map(|d| d.len() as i64).unwrap_or(-1);
    capture_diagnostic(
        db,
        &episodic.id,
        "endpoint-sync",
        candidates.len() as i64,
        episodic.content.chars().count() as i64,
        status,
        &response_json,
        dc,
    )
    .await;
    parsed
}

/// Forensic capture for one judge call (CLA-132 diagnostics). Extracts what the
/// model actually returned — stop_reason, usage, whether a `tool_use` block was
/// present, the content-block types — and records it durably, so an async
/// first-submission whiff leaves its own post-mortem. Best-effort: a failed
/// insert never touches the encode. Used by both the sync path and the batch
/// dispatch, so a failing first-submission and a succeeding re-run land side by side.
pub(crate) async fn capture_diagnostic(
    db: &D1Database,
    episodic_id: &str,
    source: &str,
    candidate_count: i64,
    content_chars: i64,
    http_status: i64,
    message: &Value,
    decision_count: i64,
) {
    let stop_reason = message
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let usage = message.get("usage");
    let usage_in = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let usage_out = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let types: Vec<&str> = message
        .get("content")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|b| b.get("type").and_then(|t| t.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let has_tool_use = types.iter().any(|t| *t == "tool_use");
    let content_types = types.join(",");
    let diag = worker_store::EncodeDiagnostic {
        episodic_id,
        source,
        candidate_count,
        content_chars,
        http_status,
        stop_reason,
        usage_in,
        usage_out,
        has_tool_use,
        content_types,
        decision_count,
        note: String::new(),
    };
    let _ = worker_store::insert_encode_diagnostic(db, &diag).await;
}

/// Pull the `encode_decision` tool call out of a Messages-format `message`
/// object into the decision list. The sync path passes the whole response (which
/// is itself a message); the async batch path passes each result's `.message`.
/// Same shape either way — one parser, both callers.
pub fn parse_encode_decisions(message: &Value) -> Result<Vec<EncodeDecision>> {
    let content = message
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
    let parsed: EncodeDecisions = serde_json::from_value(input.clone())
        .map_err(|e| worker::Error::RustError(format!("parse decisions: {}", e)))?;
    Ok(parsed.decisions)
}

/// Claude Code's compaction hook captures the harness's session-recovery
/// summary, which opens with boilerplate: "This session is being continued from
/// a previous conversation that ran out of context. The summary below covers…".
/// The encode judge reads that preamble and classifies the ENTIRE episodic as
/// recovery metadata — skipping the knowledge inside, or stamping a placeholder
/// "<UNKNOWN>" semantic that just labels it a summary (CLA-131). Four attempts
/// to prompt the judge past it failed; the signal is too strong to argue with.
/// So we remove the signal it can't read past. This strips the wrapper from the
/// JUDGE'S VIEW only — the stored episodic keeps every word. We change the
/// input, not the instruction, and never mutate the memory.
const RECOVERY_PREAMBLE_MARKER: &str =
    "This session is being continued from a previous conversation";

fn strip_recovery_preamble(content: &str) -> &str {
    let trimmed = content.trim_start();
    if !trimmed.starts_with(RECOVERY_PREAMBLE_MARKER) {
        return content;
    }
    // The preamble is the opening paragraph; the real summary begins after the
    // first blank line. Hand the judge the body. If the shape is unexpected (no
    // blank line, or nothing meaningful after it), leave it whole rather than
    // guess and risk cutting real content.
    match trimmed.split_once("\n\n") {
        Some((_, body)) if !body.trim().is_empty() => body,
        _ => content,
    }
}

fn format_for_judge(episodic: &Memory, candidates: &[Memory]) -> String {
    let mut out = String::new();
    out.push_str("NEW EPISODIC TO ENCODE:\n");
    out.push_str(&format!("id: {}\n", episodic.id));
    // A compaction's summary field is the preamble truncated — the same
    // metadata signal twice. Drop it from the judge's view when it's a wrapper;
    // the de-wrappered content below carries everything the judge needs.
    if !episodic.summary.trim_start().starts_with(RECOVERY_PREAMBLE_MARKER) {
        out.push_str(&format!("summary: {}\n", episodic.summary));
    }
    out.push_str(&format!(
        "content:\n{}\n\n",
        strip_recovery_preamble(&episodic.content)
    ));

    if candidates.is_empty() {
        out.push_str("EXISTING SEMANTICS ON THIS TOPIC: none surfaced by search.\n");
    } else {
        out.push_str("EXISTING SEMANTICS ON THESE TOPICS (nearest by hybrid search — link or revise one of these if it covers a knowledge-unit; use its id verbatim):\n");
        for c in candidates {
            out.push_str(&format!("- id: {}\n  summary: {}\n  content: {}\n", c.id, c.summary, c.content));
        }
    }

    out.push_str("\nDecompose the episodic into its distinct knowledge-units and emit one decision each. Lineage first: if an existing semantic covers a unit, link or revise it; create only when none does. Return an empty list only for a truly homeless episodic.\n");
    out
}

// ── DECOMPOSE STAGE (encode rebuild, CLA-134 sibling) ──────────────────────
//
// The monolithic judge (above) decomposes AND judges AND serialises every
// decision in one 8192-token call — which truncates mid-tool-call on a large
// capture and returns 0 usable decisions (the whiff; see the encode_diagnostics
// autopsy: stop_reason=max_tokens, usage_out=8192). The rebuild splits the two
// jobs. `decompose` does atomisation + distillation ONLY: episodic → a list of
// distilled semantic claims. Its output is terse (claim + summary, no rationale
// / action / ids), so it stays far under the ceiling. A later per-unit judge
// files each claim (link/revise/create/supersede) in its own small call.
//
// Step 0 here is the read-only PREVIEW: run decompose, return the units, write
// nothing. Lets us eyeball atomisation granularity on a known failure before any
// judging or writes exist — same calibrate-first discipline as cluster-preview.

const DECOMPOSE_SYSTEM_PROMPT: &str = "You are the decomposition stage of a memory system. One episodic memory has just been captured. Your only job is to split it into the distinct atomic knowledge-units it contains — the durable things now KNOWN — and distil each into a semantic claim. You do NOT decide where anything is filed; a later stage does that. Extract, distil, and stop.

An episodic records what happened. A semantic records what is now known. You output the second, derived from the first.

A unit of knowledge is one discrete proposition about a person, thing, or concept — an understanding that can be built on as more is learned. The test: could a future instance use this to understand the subject, or to make sense of a later conversation? If all it records is that an event occurred, it is not knowledge — leave it out.

GOOD (a unit): 'Justin is a senior iOS developer and a creative polymath — music production, photography, sim racing, piano — who builds tools to bridge what exists and what should exist (he wrote audio-analyzer because he wanted Claude to be able to hear his music).' — a durable understanding that grows as the picture develops.
BAD (not a unit): 'Justin ran out of protein bars and went to the supermarket, forgot an appointment, and almost missed it.' — just things that happened; they carry no knowledge of the subject. Leave them out.

Near-misses and negatives are not knowledge by themselves. The one exception: a near-miss IS knowledge when it disproves a hypothesis or exposes how something works — then the lesson is the unit, not the event.

GRANULARITY. One unit = one proposition. Do not fragment a single understanding into slivers (one coherent claim about Justin's iOS work is ONE unit, not four). Do not merge distinct propositions into one blob (his iOS work and his music are TWO units). Do not manufacture units to cover events — a routine day may hold zero; a dense session summary may hold many.

ENVELOPES. Some episodics are session or compaction summaries that open with recovery scaffolding ('This session is being continued from a previous conversation…'). That preamble is a wrapper, not content — the body it introduces is the real record; mine THAT. Never emit a unit that merely describes the episodic as a summary, a session, or recovery metadata; that labels the envelope instead of capturing what is inside. If a unit's content or entity comes out 'unknown' or a placeholder, you are filing the envelope — drop it.

REGISTER. Calibrate to the truth — do not inflate, do not bleach. Neutral, factual default; carry a load-bearing stance in a MIDDLE register, never escalate past what the episodic supports.

For each unit give the distilled claim as `content` (what is now known, stated so a future instance can build on it), a one-line `summary`, and the `entity` it concerns if clear. Call the decompose tool with your full list of units. If the episodic genuinely holds no durable knowledge, return an empty list.";

/// One atomic knowledge-unit extracted from an episodic — a \"proposed semantic\"
/// (distilled claim + metadata) before the per-unit judge decides where it's
/// filed. Reused downstream: Step 1 feeds each unit + its candidates to the
/// judge. Serialize for the preview endpoint; Deserialize for the tool input.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AtomicUnit {
    /// The distilled semantic claim — what is now KNOWN, not what happened.
    pub content: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub entity: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Deserialize)]
struct DecomposeUnits {
    #[serde(default)]
    units: Vec<AtomicUnit>,
}

/// Messages request body for the decompose call. Terse output by construction —
/// the unit schema has no rationale/action/id fields, only the distilled claim
/// and light metadata — so a 24k-char capture's ~30 units serialise well under
/// the 8192 ceiling that sank the monolithic judge.
fn build_decompose_request_params(episodic: &Memory) -> Value {
    // Same envelope-strip the judge uses: the recovery preamble is a wrapper.
    let content = strip_recovery_preamble(&episodic.content);
    let user_message = format!(
        "EPISODIC TO DECOMPOSE:\n\n{}\n\nExtract its distinct atomic knowledge-units.",
        content
    );

    let unit_schema = json!({
        "type": "object",
        "properties": {
            "content": { "type": "string", "description": "The distilled semantic claim — what is now KNOWN, stated so a future instance could build on it. Not a log of what happened." },
            "summary": { "type": "string", "description": "One-line summary of the claim." },
            "entity": { "type": "string", "description": "The person, thing, or concept this is about, if clear." },
            "tags": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["content", "summary"]
    });

    let tool_definition = json!({
        "name": "decompose",
        "description": "Split the episodic into its distinct atomic knowledge-units — one entry per genuine unit of durable knowledge.",
        "input_schema": {
            "type": "object",
            "properties": {
                "units": {
                    "type": "array",
                    "items": unit_schema,
                    "description": "One entry per distinct durable knowledge-unit. Empty list = the episodic holds no durable knowledge."
                }
            },
            "required": ["units"]
        }
    });

    json!({
        "model": HAIKU_MODEL,
        "max_tokens": 8192,
        "system": [{
            "type": "text",
            "text": DECOMPOSE_SYSTEM_PROMPT,
            "cache_control": { "type": "ephemeral" }
        }],
        "tools": [tool_definition],
        "tool_choice": { "type": "tool", "name": "decompose" },
        "messages": [{ "role": "user", "content": user_message }]
    })
}

/// Pull the `decompose` tool call out of a Messages-format response into the
/// unit list. Mirrors `parse_encode_decisions`.
pub fn parse_decompose_units(message: &Value) -> Result<Vec<AtomicUnit>> {
    let content = message
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
    let parsed: DecomposeUnits = serde_json::from_value(input.clone())
        .map_err(|e| worker::Error::RustError(format!("parse units: {}", e)))?;
    Ok(parsed.units)
}

/// Decompose one episodic into its atomic knowledge-units (encode rebuild). One
/// small Haiku call; writes nothing. Instrumented via `capture_diagnostic`
/// (source=\"decompose\") so a decompose-side truncation leaves its own autopsy
/// in `encode_diagnostics` — exactly how we'd catch flag #1 (decompose itself
/// hitting the ceiling). Mirrors `encode_via_claude`'s call shape.
pub async fn decompose(env: &Env, db: &D1Database, episodic: &Memory) -> Result<Vec<AtomicUnit>> {
    let token = env.secret("CLAUDE_CODE_OAUTH_TOKEN")?.to_string();
    let body = build_decompose_request_params(episodic);

    let mut headers = Headers::new();
    if token.starts_with("sk-ant-api") {
        headers.set("x-api-key", &token)?;
    } else {
        headers.set("Authorization", &format!("Bearer {}", token))?;
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

    let status = resp.status_code() as i64;
    let body_text = resp.text().await?;
    let response_json: Value = serde_json::from_str(&body_text)
        .map_err(|e| worker::Error::RustError(format!("parse response: {}", e)))?;
    let parsed = parse_decompose_units(&response_json);
    let n = parsed.as_ref().map(|u| u.len() as i64).unwrap_or(-1);
    capture_diagnostic(
        db,
        &episodic.id,
        "decompose",
        -1,
        episodic.content.chars().count() as i64,
        status,
        &response_json,
        n,
    )
    .await;
    parsed
}

// ── PER-UNIT JUDGE (encode rebuild Step 1) ─────────────────────────────────
//
// The filing half the monolith did inline. Given ONE distilled unit + the
// semantics nearest it, decide where it belongs (link/revise/create/supersede/
// skip) in a single small call — one decision, max_tokens 2048, truncation
// structurally impossible. `encode_decomposed` ties decompose + per-unit search
// + this judge together; commit=false dry-runs it (judge, return, write nothing).

const ENCODE_UNIT_SYSTEM_PROMPT: &str = "You are the filing stage of a memory system. One distilled knowledge-unit — a single semantic claim, already extracted from a new episodic — is given to you, with the existing semantics nearest it by search. Decide where this one unit belongs, and keep the lineage TRUE.

Lineage is contribution mass: how many episodics feed a semantic, and how recently. The mass only means something if it is honest. A genuine match you fail to link is a lost vote; a link or revise to the wrong semantic is a forged vote. File this unit where it genuinely belongs: nowhere else, and never nowhere when it belongs somewhere.

THE CANDIDATES are retrieval output, not a verdict on relevance — suggestions to check, not homes to choose between. Often none of them holds this unit; that is normal and fine. Search surfaces broad, multi-topic semantics for almost anything, because a blob that covers many things is near everything — those are the most tempting and the most wrong. A broad neighbour is rarely your unit's home.

DECIDE:
1. Does a candidate hold the SAME KNOWLEDGE-UNIT — the same single proposition about the same subject? Same topic is not enough ('both concern his health' is a shared topic, not a shared unit); same project is not enough either. Two hard tests, and BOTH must pass: (a) you can state the shared proposition in ONE plain sentence — if it takes a paragraph to argue, it is not the same unit; (b) the candidate is not BROADER than your unit. A candidate whose content spans more than your single unit — a multi-topic summary, a semantic about several distinct things — is a SUPERSET, not a match; folding your narrow unit into it makes a worse blob and forges a vote. When the candidate is broader than your unit, the answer is CREATE — never link or revise.
   - Holds it wholly, nothing new here → LINK (the edge is the signal the knowledge recurred; add it though you change nothing).
   - Holds it and this unit genuinely deepens or extends it → REVISE (fold in; state the genuine link in rationale).
   - Held it but this unit overturns or moves past it → SUPERSEDE (flavours below).
2. No candidate holds it → CREATE. The normal outcome for genuinely new knowledge, not a failure.
3. On a second look this unit is not durable, reusable knowledge after all → SKIP. Rare (it was already extracted as knowledge), but allowed.

THE GOVERNING ASYMMETRY. When you are anything short of certain a candidate holds the SAME single unit, CREATE. This is not a tie-breaker — it is the default. A duplicate create is harmless: a later consolidation pass merges near-duplicate semantics automatically. A wrong link or revise is a forged vote AND an overwrite that nothing undoes — it corrupts a memory's lineage and conflates distinct knowledge permanently. The errors are not symmetric, so do not weigh them as a balanced choice: lean hard to create, and link or revise ONLY when both tests above pass cleanly and you would stake the lineage on it.

SUPERSEDE flavours — set supersede_flavour:
- 'correction' — a credible new source shows the old was WRONG (credibility decides, not frequency).
- 'progression' — the old was TRUE but is a state since MOVED PAST on the same life-axis (rented a flat → bought a house: not false, just history).
Either case: the current view goes in semantic_content, the old semantic's id in existing_id. The old is retired but kept recallable.

CONFLICT. If this unit clashes with a candidate and both positions are still credible and unresolved, do NOT pick a winner: REVISE to hold both, stating the uncertainty plainly. The conflict itself is knowledge.

CONTENT. For create: semantic_content is the claim, refined only for clarity. For revise/supersede: semantic_content is the merged/updated knowledge. For link: none needed. Calibrate to the truth — do not inflate, do not bleach. Everything you write derives from this unit; never assert what it does not support.

edge_weight: how much this unit's source episodic contributes to the semantic — primary (a core source), secondary (supporting), or mention (passing).

Respond by calling the file_unit tool with your single decision.";

/// Candidate semantics for ONE unit — hybrid retrieval like
/// `find_candidate_semantics`, but a unit is a single focused proposition, so a
/// single embed of its claim suffices (no content-windowing). Filtered to
/// semantics; get_many already excludes superseded.
async fn find_candidates_for_unit(
    env: &Env,
    db: &D1Database,
    unit: &AtomicUnit,
) -> Result<Vec<Memory>> {
    let oversample = CANDIDATE_K * 4;

    let vector_ranking: Vec<String> = match worker_embed::embed_query(env, &unit.content).await {
        Ok(emb) => worker_vectorize::query_top_k(env, &emb, oversample)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.id)
            .collect(),
        Err(_) => Vec::new(),
    };

    // FTS on the unit summary (fall back to the claim if the summary is thin).
    let fts_text = if unit.summary.trim().is_empty() {
        unit.content.as_str()
    } else {
        unit.summary.as_str()
    };
    let fts_ranking: Vec<String> = match crate::hybrid::build_fts_query(fts_text) {
        Some(expr) => worker_store::fts_search(db, &expr, oversample)
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };

    let fused = crate::hybrid::rrf_fuse(
        &fts_ranking,
        &vector_ranking,
        1.0,
        crate::hybrid::DEFAULT_RRF_K,
    );
    let want = CANDIDATE_K as usize;
    let probe_ids: Vec<&str> = fused
        .iter()
        .take(want * 4)
        .map(|(id, _)| id.as_str())
        .collect();
    let mut by_id: std::collections::HashMap<String, Memory> = worker_store::get_many(db, &probe_ids)
        .await?
        .into_iter()
        .filter(|m| matches!(&m.memory_type, MemoryType::Semantic))
        .map(|m| (m.id.clone(), m))
        .collect();
    Ok(fused
        .iter()
        .filter_map(|(id, _)| by_id.remove(id))
        .take(want)
        .collect())
}

/// Messages request body for filing ONE unit. The tool returns a single decision
/// object (the `EncodeDecision` shape) — not a list — so max_tokens 2048 is
/// ample and truncation cannot occur.
fn build_unit_judge_params(unit: &AtomicUnit, candidates: &[Memory]) -> Value {
    let mut user = String::new();
    user.push_str("KNOWLEDGE-UNIT TO FILE:\n");
    user.push_str(&format!("content: {}\n", unit.content));
    if !unit.summary.trim().is_empty() {
        user.push_str(&format!("summary: {}\n", unit.summary));
    }
    if let Some(e) = &unit.entity {
        user.push_str(&format!("entity: {}\n", e));
    }
    user.push('\n');
    if candidates.is_empty() {
        user.push_str("EXISTING SEMANTICS NEAREST THIS UNIT: none surfaced by search.\n");
    } else {
        user.push_str("EXISTING SEMANTICS NEAREST THIS UNIT (link/revise/supersede one ONLY if it holds the SAME knowledge-unit; use its id verbatim):\n");
        for c in candidates {
            user.push_str(&format!("- id: {}\n  summary: {}\n  content: {}\n", c.id, c.summary, c.content));
        }
    }
    user.push_str("\nDecide where this one unit belongs. Lineage first; create only when no candidate holds it.");

    let decision_schema = json!({
        "type": "object",
        "properties": {
            "action": { "type": "string", "enum": ["skip", "link", "revise", "create", "supersede"] },
            "existing_id": { "type": "string", "description": "Full UUID of the existing semantic. Required for link, revise, and supersede." },
            "semantic_content": { "type": "string", "description": "The distilled semantic knowledge. Required for revise, create, and supersede." },
            "summary": { "type": "string", "description": "One-line summary. Required for revise and create." },
            "entity": { "type": "string" },
            "tags": { "type": "array", "items": { "type": "string" } },
            "edge_weight": { "type": "string", "enum": ["primary", "secondary", "mention"], "description": "How much this unit's source episodic contributes to the semantic." },
            "supersede_flavour": { "type": "string", "enum": ["correction", "progression"], "description": "For supersede only — 'correction': the old was wrong; 'progression': the old was true but is a state since moved past." },
            "rationale": { "type": "string", "description": "Why this action. For link/revise/supersede this MUST name the concept this unit and the targeted semantic actually share and how this unit deepens, extends, or overturns it. If you cannot state a plain genuine link, the match is spurious — the action is create or skip, never a write over that memory." }
        },
        "required": ["action", "rationale"]
    });

    let tool_definition = json!({
        "name": "file_unit",
        "description": "File this one knowledge-unit into semantic memory: link / revise / create / supersede / skip. Lineage accuracy is the priority.",
        "input_schema": decision_schema
    });

    json!({
        "model": HAIKU_MODEL,
        // One decision, no fan-out — 2048 is ample; the per-unit call is what
        // makes the rebuild truncation-proof.
        "max_tokens": 2048,
        "system": [{
            "type": "text",
            "text": ENCODE_UNIT_SYSTEM_PROMPT,
            "cache_control": { "type": "ephemeral" }
        }],
        "tools": [tool_definition],
        "tool_choice": { "type": "tool", "name": "file_unit" },
        "messages": [{ "role": "user", "content": user }]
    })
}

/// Pull the single `file_unit` decision out of a Messages response. Unlike
/// `parse_encode_decisions` the tool input IS the decision object directly.
fn parse_unit_decision(message: &Value) -> Result<EncodeDecision> {
    let content = message
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
    let decision: EncodeDecision = serde_json::from_value(input.clone())
        .map_err(|e| worker::Error::RustError(format!("parse unit decision: {}", e)))?;
    Ok(decision)
}

/// The per-unit Haiku filing call. Inline auth mirrors `encode_via_claude` /
/// `decompose` (the project's accepted one-block duplication). Instrumented via
/// `capture_diagnostic(source="unit-judge")` so each unit's call leaves an
/// autopsy — confirming the per-unit calls stay small.
async fn judge_unit(
    env: &Env,
    db: &D1Database,
    episodic_id: &str,
    unit: &AtomicUnit,
    candidates: &[Memory],
) -> Result<EncodeDecision> {
    let token = env.secret("CLAUDE_CODE_OAUTH_TOKEN")?.to_string();
    let body = build_unit_judge_params(unit, candidates);

    let mut headers = Headers::new();
    if token.starts_with("sk-ant-api") {
        headers.set("x-api-key", &token)?;
    } else {
        headers.set("Authorization", &format!("Bearer {}", token))?;
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

    let status = resp.status_code() as i64;
    let body_text = resp.text().await?;
    let response_json: Value = serde_json::from_str(&body_text)
        .map_err(|e| worker::Error::RustError(format!("parse response: {}", e)))?;
    let parsed = parse_unit_decision(&response_json);
    let dc = parsed.as_ref().map(|_| 1i64).unwrap_or(-1);
    capture_diagnostic(
        db,
        episodic_id,
        "unit-judge",
        candidates.len() as i64,
        unit.content.chars().count() as i64,
        status,
        &response_json,
        dc,
    )
    .await;
    parsed
}

/// One unit's filing result for the decomposed-encode probe — the judge's
/// decision plus what it saw, enough to eyeball whether link/revise/create
/// choices are genuine before any writes.
#[derive(Serialize)]
pub struct UnitFiling {
    pub content: String,
    pub entity: Option<String>,
    pub action: String,
    /// existing_id (8-char) for link/revise/supersede.
    pub target: Option<String>,
    pub rationale: Option<String>,
    /// Some(id8) only when actually dispatched (commit=true).
    pub committed_semantic: Option<String>,
    /// 8-char ids the judge was shown — cross-ref a link target against these.
    pub candidate_ids: Vec<String>,
}

/// Decomposed encode over one episodic (rebuild Step 1): decompose → per-unit
/// candidate search → per-unit single-decision judge → (if `commit`) dispatch.
/// `commit=false` is a DRY RUN — judges every unit and returns the filings but
/// writes nothing, so the judge's choices can be eyeballed on a known episodic
/// before trusting it against the sacred store. Every model call here is small;
/// the truncation that sank the monolith cannot happen.
pub async fn encode_decomposed(
    env: &Env,
    db: &D1Database,
    episodic: &Memory,
    commit: bool,
    limit: Option<usize>,
) -> Result<Vec<UnitFiling>> {
    let units = decompose(env, db, episodic).await?;
    let take = limit.unwrap_or(units.len());
    let mut filings = Vec::with_capacity(take.min(units.len()));

    for unit in units.iter().take(take) {
        let candidates = find_candidates_for_unit(env, db, unit)
            .await
            .unwrap_or_default();
        let decision = match judge_unit(env, db, &episodic.id, unit, &candidates).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("unit judge failed for {}: {:?}", &episodic.id[..episodic.id.len().min(8)], e);
                continue;
            }
        };

        let mut committed = None;
        if commit {
            // Fall back to the unit's distilled claim when the judge emits a bare
            // create/supersede with no content, so a terse response still writes.
            let mut d = decision.clone();
            let empty = d
                .semantic_content
                .as_deref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true);
            if empty && matches!(d.action.to_lowercase().as_str(), "create" | "supersede") {
                d.semantic_content = Some(unit.content.clone());
            }
            if let Ok(outcome) = dispatch_one(env, db, episodic, &d).await {
                committed = outcome
                    .semantic_id
                    .map(|id| id[..id.len().min(8)].to_string());
            }
        }

        filings.push(UnitFiling {
            content: unit.content.clone(),
            entity: unit.entity.clone(),
            action: decision.action.clone(),
            target: decision
                .existing_id
                .as_ref()
                .map(|s| s[..s.len().min(8)].to_string()),
            rationale: decision.rationale.clone(),
            committed_semantic: committed,
            candidate_ids: candidates
                .iter()
                .map(|c| c.id[..c.id.len().min(8)].to_string())
                .collect(),
        });
    }

    // Only mark integrated on a FULL commit (no unit limit) that filed something
    // (or a small episodic) — same substantial-episodic guard as the monolith
    // path. A limited run is a partial probe; never bank it as integrated.
    if commit && limit.is_none() {
        const SUBSTANTIAL_EPISODIC_CHARS: usize = 4000;
        let filed = filings.iter().any(|f| f.committed_semantic.is_some());
        if filed || episodic.content.len() <= SUBSTANTIAL_EPISODIC_CHARS {
            worker_store::mark_integrated(db, &episodic.id).await?;
        }
    }

    Ok(filings)
}

/// Judge and dispatch ONE atomic unit against the live store — the production
/// per-unit step (CLA-134). Find candidates → judge → dispatch_one, linked to
/// the source episodic. The queue's EncodeUnit handler calls this, one unit per
/// message, so no single consumer invocation runs the whole multi-call encode
/// (the wall-time wall the old batch path was built around). Errors propagate so
/// the queue retries this unit; a retry may duplicate a semantic, which CSCC
/// merges (over is safe). Never re-writes the episodic.
pub async fn encode_unit(
    env: &Env,
    db: &D1Database,
    episodic: &Memory,
    unit: &AtomicUnit,
) -> Result<()> {
    let candidates = find_candidates_for_unit(env, db, unit)
        .await
        .unwrap_or_default();
    let mut decision = judge_unit(env, db, &episodic.id, unit, &candidates).await?;
    // Fall back to the unit's distilled claim when the judge emits a bare
    // create/supersede with no content (same guard as the dry-run commit path).
    let empty = decision
        .semantic_content
        .as_deref()
        .map(|s| s.trim().is_empty())
        .unwrap_or(true);
    if empty && matches!(decision.action.to_lowercase().as_str(), "create" | "supersede") {
        decision.semantic_content = Some(unit.content.clone());
    }
    dispatch_one(env, db, episodic, &decision).await?;
    Ok(())
}

/// Batch trigger (MVP): process up to `limit` unintegrated episodics. The
/// per-write queue consumer calls `encode_one` directly; this is the batch
/// form for a cron or manual invocation. A failed item is left unintegrated
/// (no mark) so the next pass retries it.
pub async fn run_encode_batch(env: &Env, db: &D1Database, limit: u32) -> Result<Vec<EncodeOutcome>> {
    let episodics = worker_store::find_unintegrated_episodics(db, limit).await?;
    let mut outcomes = Vec::new();
    for ep in &episodics {
        match encode_one(env, db, ep).await {
            Ok(o) => outcomes.push(o),
            Err(e) => tracing::warn!("encode_one failed for {}: {:?}", &ep.id[..8], e),
        }
    }
    Ok(outcomes)
}

/// A capture queued for asynchronous write + encode. Produced by the `/encode`
/// hook endpoint and the `reflect` MCP tool — both run on any client (desktop,
/// phone, web, CLI), which is the point: capture no longer depends on the
/// CLI-only PostCompact hook. Consumed by the queue handler in `lib.rs`, which
/// calls `process_capture`. Decouples a fast tool response from the slow Haiku
/// encode, with durable delivery + automatic retries.
#[derive(Serialize, Deserialize)]
pub struct CaptureMessage {
    pub content: String,
    pub summary: String,
    #[serde(default)]
    pub entity: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub recorded_by: Option<String>,
}

/// Everything that rides the capture queue (CLA-132). `Capture` is a new episodic
/// to write then submit for encoding; `PollBatch` is a deferred check on a
/// submitted Anthropic batch, re-enqueued with a delay until the batch ends. One
/// queue, one consumer, dispatched by the `kind` tag. Internally tagged so a
/// `Capture`'s fields sit flat alongside `"kind":"capture"` — which also lets the
/// consumer fall back to reading a bare (pre-CLA-132) `CaptureMessage`.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QueueMessage {
    Capture(CaptureMessage),
    /// One atomic unit to judge + dispatch (encode rebuild, CLA-134). Decompose
    /// fans these out one-per-unit so no single consumer invocation runs the
    /// whole multi-call encode; each is retried independently.
    EncodeUnit { episodic_id: String, unit: AtomicUnit },
    PollBatch { batch_id: String, attempt: u32 },
}

/// Write a captured episodic to the store and return it. Create is the durable,
/// idempotency-critical step: if it fails the message retries, and since no row
/// was written there is no duplicate. Encoding is NOT done here — the queue
/// consumer submits the returned episodic to an async batch (CLA-132), keeping
/// the slow Haiku judgment off this function's failure boundary so that a retry
/// can never re-write the episodic.
pub async fn process_capture(db: &D1Database, msg: &CaptureMessage) -> Result<Memory> {
    worker_store::create_memory_with_provenance(
        db,
        MemoryType::Episodic,
        msg.content.clone(),
        msg.summary.clone(),
        msg.entity.clone(),
        msg.tags.clone(),
        msg.recorded_by.clone(),
    )
    .await
}

/// Decompose an episodic and fan each unit out as its own EncodeUnit queue
/// message, then mark the episodic integrated (CLA-134). One small decompose
/// call happens here; the per-unit judging then runs in separate tiny consumer
/// invocations, so no single invocation runs the whole 18–30-call encode and
/// overruns wall-time. Integration is marked once the units are enqueued —
/// per-unit failures retry per message, finer-grained and safer than
/// re-decomposing the whole episodic (which would duplicate units). Returns the
/// number of units enqueued.
pub async fn decompose_and_fan_out(
    env: &Env,
    db: &D1Database,
    episodic: &Memory,
) -> Result<usize> {
    let units = decompose(env, db, episodic).await?;
    let mut enqueued = 0usize;
    for unit in &units {
        let msg = QueueMessage::EncodeUnit {
            episodic_id: episodic.id.clone(),
            unit: unit.clone(),
        };
        if env.queue("CAPTURE_QUEUE")?.send(msg).await.is_ok() {
            enqueued += 1;
        }
    }
    if enqueued == units.len() {
        worker_store::mark_integrated(db, &episodic.id).await?;
    }
    Ok(enqueued)
}
