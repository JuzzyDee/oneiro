// worker_adas.rs — Atomic Decomposition And Separation (CLA-134): the SPLIT half
// of defrag, CSCC's mirror.
//
// CSCC merges near-duplicate semantics (over-fragmentation). ADAS splits
// CONFLATED ones — a single row that's secretly several subjects, usually a
// pre-atomic leftover from before decompose-first encode (canonical case:
// 886d428f, a collaboration principle that also name-drops four technical
// topics). Left alone, a conflated semantic over-creates in orientation (the
// distiller mines multiple axes from one row) and muddies recall.
//
// SEPARATION reuses the encode `decompose` Haiku call (semantic, whole-content —
// NOT mechanical chunking). But DETECTION must be PROCEDURAL: you can't Haiku-poll
// the whole store asking "are you conflated?" — same reason CSCC's detector is a
// cheap cosine threshold, not a model call. This file is the read-only DETECTOR
// PREVIEW: the calibrate-first step, mirroring CSCC's cluster-preview. Two free,
// in-Worker signals, zero Haiku:
//
//   orient_axes — how many DISTINCT orientation axes a semantic feeds
//                 (semantic_orientation edges). The sharpest signal: the orient
//                 judge ALREADY found it multi-subject. >= 2 is the conflation tell.
//                 A direct semantic readout, not a syntactic proxy.
//   length      — char count. Cheap recall for the backlog that hasn't fed orient
//                 yet (a blob is long before it's ever distilled).
//
// The detector is allowed to be blunt — it's a high-recall PRE-FILTER, and Haiku
// is the verdict that splits only the flagged few (false positives cost one
// wasted call; misses get caught next pass). Same blunt-detector + smart-judge
// shape CSCC proved today. This preview ranks candidates by both signals so the
// thresholds get calibrated on the real store before any separation is built.

#![cfg(target_family = "wasm")]

use serde::Deserialize;
use serde_json::{json, Value};
use worker::{D1Database, Env, Fetch, Headers, Method, Request, RequestInit, Result};

const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";

/// Read-only conflation-candidate preview (CLA-134, ADAS). NO Haiku, NO writes.
/// Ranks live semantics by (orient_axes, length) and returns the signal
/// distribution plus the candidates over the thresholds, so the detector can be
/// calibrated before separation exists.
pub async fn split_candidate_preview(
    db: &D1Database,
    min_axes: i64,
    min_len: i64,
    limit: usize,
) -> Result<Value> {
    #[derive(serde::Deserialize)]
    struct Row {
        id: String,
        summary: String,
        len: i64,
        orient_axes: i64,
    }
    // One pass over the live semantic layer. orient_axes is a correlated count of
    // the distinct orientation axes each semantic feeds — the over-creation signal
    // as a pure query, no model.
    let rows: Vec<Row> = db
        .prepare(
            "SELECT m.id AS id,
                    COALESCE(m.summary, '') AS summary,
                    LENGTH(COALESCE(m.content, '')) AS len,
                    (SELECT COUNT(DISTINCT cl.parent_id)
                       FROM consolidation_lineage cl
                      WHERE cl.source_id = m.id
                        AND cl.edge_type = 'semantic_orientation') AS orient_axes
             FROM memories m
             WHERE m.memory_type = 'semantic' AND m.superseded = 0
             ORDER BY orient_axes DESC, len DESC",
        )
        .all()
        .await?
        .results()?;

    let total = rows.len();
    // Distribution of the sharp signal, so we can see where the conflation line is.
    let (mut a0, mut a1, mut a2, mut a3plus) = (0usize, 0usize, 0usize, 0usize);
    let mut max_len = 0i64;
    for r in &rows {
        match r.orient_axes {
            0 => a0 += 1,
            1 => a1 += 1,
            2 => a2 += 1,
            _ => a3plus += 1,
        }
        if r.len > max_len {
            max_len = r.len;
        }
    }

    // Candidates: flagged by EITHER signal (high recall — Haiku confirms later).
    let detail: Vec<Value> = rows
        .iter()
        .filter(|r| r.orient_axes >= min_axes || r.len >= min_len)
        .take(limit)
        .map(|r| {
            json!({
                "id": &r.id[..r.id.len().min(8)],
                "orient_axes": r.orient_axes,
                "len": r.len,
                "summary": r.summary,
            })
        })
        .collect();

    Ok(json!({
        "total_semantics": total,
        "min_axes": min_axes,
        "min_len": min_len,
        "axes_distribution": { "0": a0, "1": a1, "2": a2, "3+": a3plus },
        "max_len": max_len,
        "candidates": detail.len(),
        "detail": detail,
    }))
}

// ─────────────────────────────────────────────────────────────────────────────
// THE SPLIT JUDGE — the discriminator the calibration proved we need. The cheap
// signals (over-creation, length) flag the crown jewels right alongside the blobs
// because they measure WEIGHT, not conflation. Only reading the whole content
// separates "one rich subject" from "several unrelated subjects stapled together"
// — so Haiku reads it whole. KEEP-biased hard: a wrong split destroys a memory,
// and the highest-ranked candidates are the most relational, least replaceable
// rows in the store. Tune on dry-run output like the encode/merge judges.
// ─────────────────────────────────────────────────────────────────────────────
const SPLIT_JUDGE_PROMPT: &str = "You are the split judge for the semantic layer of a memory system. You are shown ONE semantic memory's full content and decide ONE thing: is it a single coherent subject (KEEP), or several genuinely UNRELATED subjects fused into one row (SPLIT)?

Most semantics are ONE subject. A few — usually older rows written before the system learned to decompose cleanly — are CONFLATED: unrelated subjects stapled together because they happened in the same moment.

CONFLATED (split) = the content covers subjects with no business sharing a memory. Example shape: a reflection on someone's WORKING STYLE that also states which API was chosen, that a UI widget shipped, and that governance is collaborative — four unrelated topics in one row. If you split, name the distinct subjects.

KEEP — and this half matters more, because a wrong split permanently destroys a memory:
- One subject told richly, from many angles, at length, is STILL ONE SUBJECT. Depth and length are NOT conflation.
- A subject plus its own causes, implications, or consequences is ONE subject.
- Anything RELATIONAL, EMOTIONAL, or about a person's WELLBEING or IDENTITY is almost always one subject even when it touches many facets of a life — and these are the most load-bearing, least replaceable rows in the store. The bar to call one of these conflated is EXTREMELY high. When in doubt on anything relational, emotional, identity, or wellbeing: KEEP.

The default is KEEP. Choose SPLIT only when you can name two or more subjects that are genuinely UNRELATED and would each stand cleaner alone. Respond with the split_decision tool.";

#[derive(Deserialize)]
struct SplitVerdict {
    conflated: bool,
    #[serde(default)]
    subjects: Vec<String>,
    #[serde(default)]
    reason: Option<String>,
}

/// Ask Haiku whether one semantic is a single subject (keep) or several unrelated
/// ones (split). Whole content, no chunking.
async fn judge_split(env: &Env, content: &str) -> Result<SplitVerdict> {
    let user = format!(
        "THE SEMANTIC MEMORY TO JUDGE (full content):\n\n{}\n\nCall split_decision. Default to conflated=false; the bar for splitting anything relational, emotional, identity, or wellbeing-related is extremely high.",
        content
    );
    let tool = json!({
        "name": "split_decision",
        "description": "Decide if this semantic is one coherent subject (keep) or several unrelated subjects (split).",
        "input_schema": {
            "type": "object",
            "properties": {
                "conflated": { "type": "boolean", "description": "true ONLY if the content fuses 2+ genuinely UNRELATED subjects. Default false. Relational/emotional/identity/wellbeing content is almost always false." },
                "subjects": { "type": "array", "items": { "type": "string" }, "description": "If conflated, the distinct atomic subjects to separate into — one short phrase each." },
                "reason": { "type": "string", "description": "Why. For keep, name the single subject. For split, why the subjects are unrelated." }
            },
            "required": ["conflated", "reason"]
        }
    });
    let input = call_haiku_tool(env, SPLIT_JUDGE_PROMPT, "split_decision", tool, user, 1024).await?;
    serde_json::from_value(input)
        .map_err(|e| worker::Error::RustError(format!("parse split_decision: {}", e)))
}

/// DRY-RUN split-judge calibrator (CLA-134, ADAS). Pre-filters by the cheap
/// signals (over-creation OR length), then has Haiku read each candidate WHOLE
/// and judge keep-vs-split. NO writes. Surfaces `protected` (does the candidate
/// feed a core/pinned orientation axis?) alongside the verdict, so we can SEE the
/// crown-jewel guard interact — a `protected && conflated` row is exactly where
/// the execute-time guard would override the judge. Calibrate the prompt on this
/// output before any separation is built.
pub async fn split_judge_preview(
    env: &Env,
    db: &D1Database,
    min_axes: i64,
    min_len: i64,
    limit: usize,
) -> Result<Value> {
    #[derive(Deserialize)]
    struct Cand {
        id: String,
        summary: String,
        content: String,
        len: i64,
        orient_axes: i64,
        core_axes: i64,
    }
    // CTE so the WHERE can reference the computed signals. core_axes = how many
    // PINNED orientation axes this semantic feeds — the protection flag.
    let rows: Vec<Cand> = db
        .prepare(
            "WITH cand AS (
                 SELECT m.id AS id,
                        COALESCE(m.summary,'') AS summary,
                        COALESCE(m.content,'') AS content,
                        LENGTH(COALESCE(m.content,'')) AS len,
                        (SELECT COUNT(DISTINCT cl.parent_id) FROM consolidation_lineage cl
                          WHERE cl.source_id = m.id AND cl.edge_type = 'semantic_orientation') AS orient_axes,
                        (SELECT COUNT(*) FROM consolidation_lineage cl JOIN memories o ON o.id = cl.parent_id
                          WHERE cl.source_id = m.id AND cl.edge_type = 'semantic_orientation' AND o.core = 1) AS core_axes
                 FROM memories m
                 WHERE m.memory_type = 'semantic' AND m.superseded = 0
             )
             SELECT * FROM cand
             WHERE orient_axes >= ? OR len >= ?
             ORDER BY orient_axes DESC, len DESC
             LIMIT ?",
        )
        .bind(&[
            (min_axes as f64).into(),
            (min_len as f64).into(),
            (limit as u32).into(),
        ])?
        .all()
        .await?
        .results()?;

    let mut detail: Vec<Value> = Vec::new();
    let (mut conflated_n, mut coherent_n, mut protected_flagged) = (0usize, 0usize, 0usize);
    for c in &rows {
        let v = judge_split(env, &c.content).await?;
        let protected = c.core_axes > 0;
        if v.conflated {
            conflated_n += 1;
            if protected {
                protected_flagged += 1;
            }
        } else {
            coherent_n += 1;
        }
        detail.push(json!({
            "id": &c.id[..c.id.len().min(8)],
            "orient_axes": c.orient_axes,
            "len": c.len,
            "protected": protected,
            "conflated": v.conflated,
            "subjects": v.subjects,
            "summary": c.summary,
            "reason": v.reason,
        }));
    }

    Ok(json!({
        "judged": rows.len(),
        "min_axes": min_axes,
        "min_len": min_len,
        "conflated": conflated_n,
        "coherent": coherent_n,
        "protected_but_flagged_conflated": protected_flagged,
        "dry_run": true,
        "detail": detail,
    }))
}

/// One forced-tool Haiku call → the validated tool `input`. Token-type aware,
/// cached system prompt. Mirrors the helpers in worker_cscc / worker_orient_distill.
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
