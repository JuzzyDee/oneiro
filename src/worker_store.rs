// worker_store.rs — D1-backed memory store for the Cloudflare Worker.
//
// Mirrors a subset of store.rs's public API but uses Cloudflare D1
// instead of rusqlite. Lives only on wasm32; the native bins keep using
// store.rs unchanged during the migration.
//
// Embeddings DO NOT live in this table — Vectorize owns the vector
// index, keyed by memory_id. Recall is a two-step pattern:
//
//   1. Vectorize.query(embedding) → top-k memory_ids
//   2. SELECT … FROM memories WHERE id IN (?, ?, …)
//
// Phase 2b.1 implemented the minimum viable set:
//
//   create_memory_with_provenance — INSERT a memory row
//   get                            — SELECT one by id
//
// Embedding generation (Workers AI) and Vectorize integration land in
// CLA-84 phases 3 + 4. Subsequent commits add `find_neighbours`,
// `touch`, `forget`, `consolidate`, `reframe`, `record_co_activation`,
// `apply_decay`, and the image-handling paths. CLA-103 retired
// `recall_active` (was only used by the now-removed `review` tool).

use crate::memory::{Memory, MemoryType};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use worker::{Bucket, D1Database, Result};

/// Row representation matching the D1 `memories` table schema. Serde
/// deserializes D1 rows into this; `into_memory()` converts to the
/// in-process `Memory` type the rest of the codebase uses. Kept
/// separate from `Memory` because:
///
///   * D1 stores timestamps as TEXT (RFC3339), `Memory.created_at`
///     is `DateTime<Utc>` — round trip via string.
///   * D1 stores tags as a JSON-encoded string, `Memory.tags` is
///     `Vec<String>` — round trip via serde_json.
///   * `Memory.embedding` doesn't exist in D1 (Vectorize owns vectors).
#[derive(Debug, serde::Deserialize)]
struct MemoryRow {
    id: String,
    memory_type: String,
    content: String,
    summary: String,
    created_at: String,
    last_accessed: String,
    access_count: u32,
    strength: f64,
    stability: f64,
    entity: Option<String>,
    tags: String,
    image_hash: Option<String>,
    image_mime: Option<String>,
    recorded_by: Option<String>,
}

impl MemoryRow {
    fn into_memory(self) -> Memory {
        let created_at = DateTime::parse_from_rfc3339(&self.created_at)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        let last_accessed = DateTime::parse_from_rfc3339(&self.last_accessed)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        let memory_type = MemoryType::from_str(&self.memory_type).unwrap_or(MemoryType::Episodic);
        let tags: Vec<String> = serde_json::from_str(&self.tags).unwrap_or_default();

        Memory {
            id: self.id,
            memory_type,
            content: self.content,
            summary: self.summary,
            created_at,
            last_accessed,
            access_count: self.access_count,
            strength: self.strength,
            stability: self.stability,
            entity: self.entity,
            tags,
            embedding: None, // Vectorize owns the vector — never populated from D1
            image_hash: self.image_hash,
            image_mime: self.image_mime,
            recorded_by: self.recorded_by,
        }
    }
}

/// INSERT a fresh memory row. Caller supplies provenance (`recorded_by`)
/// from the auth context — never trust a client-supplied value.
///
/// Returns the constructed `Memory` so callers can attach the embedding
/// to Vectorize using the same id (phase 4 wiring).
pub async fn create_memory_with_provenance(
    db: &D1Database,
    memory_type: MemoryType,
    content: String,
    summary: String,
    entity: Option<String>,
    tags: Vec<String>,
    recorded_by: Option<String>,
) -> Result<Memory> {
    let now = Utc::now();
    let id = Uuid::new_v4().to_string();
    let tags_json = serde_json::to_string(&tags).unwrap_or_else(|_| "[]".to_string());
    let stability = memory_type.base_stability();

    db.prepare(
        "INSERT INTO memories
            (id, memory_type, content, summary, created_at, last_accessed,
             access_count, strength, stability, entity, tags, image_hash,
             image_mime, recorded_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&[
        id.clone().into(),
        memory_type.as_str().into(),
        content.clone().into(),
        summary.clone().into(),
        now.to_rfc3339().into(),
        now.to_rfc3339().into(),
        0i32.into(),
        1.0_f64.into(),
        stability.into(),
        match &entity {
            Some(e) => e.clone().into(),
            None => worker::wasm_bindgen::JsValue::NULL,
        },
        tags_json.into(),
        worker::wasm_bindgen::JsValue::NULL, // image_hash — phase 2b.N adds image writes
        worker::wasm_bindgen::JsValue::NULL, // image_mime
        match &recorded_by {
            Some(r) => r.clone().into(),
            None => worker::wasm_bindgen::JsValue::NULL,
        },
    ])?
    .run()
    .await?;

    Ok(Memory {
        id,
        memory_type,
        content,
        summary,
        created_at: now,
        last_accessed: now,
        access_count: 0,
        strength: 1.0,
        stability,
        entity,
        tags,
        embedding: None,
        image_hash: None,
        image_mime: None,
        recorded_by,
    })
}

/// Fetch one memory by exact id. Returns `Ok(None)` when not found.
pub async fn get(db: &D1Database, id: &str) -> Result<Option<Memory>> {
    let row = db
        .prepare(
            "SELECT id, memory_type, content, summary, created_at, last_accessed,
                    access_count, strength, stability, entity, tags, image_hash,
                    image_mime, recorded_by
             FROM memories
             WHERE id = ?",
        )
        .bind(&[id.into()])?
        .first::<MemoryRow>(None)
        .await?;
    Ok(row.map(MemoryRow::into_memory))
}

/// INSERT a memory record verbatim — preserves the caller's id,
/// timestamps, strength, all fields exactly. Used by /admin/import
/// during data migration (CLA-84 phase 8) so the migrated rows keep
/// their original ids (the 8-char prefixes you've memorized) and their
/// original chronology.
///
/// This bypasses every "server-controlled" rule the regular create
/// paths enforce — that's intentional, because the data being imported
/// came from a trusted local DB. Reuse outside the migration path
/// requires careful thought; the endpoint should stay admin-only.
pub async fn insert_memory_verbatim(db: &D1Database, m: &Memory) -> Result<()> {
    let tags_json = serde_json::to_string(&m.tags).unwrap_or_else(|_| "[]".to_string());
    // `ON CONFLICT(id) DO NOTHING` makes the migration idempotent at the
    // SQL level — re-runs silently skip already-imported rows without
    // surfacing PK conflicts as errors. Other constraint violations
    // (NOT NULL, datatype mismatch, etc.) still fail normally and
    // surface via the success() check below.
    let result = db
        .prepare(
            "INSERT INTO memories
            (id, memory_type, content, summary, created_at, last_accessed,
             access_count, strength, stability, entity, tags, image_hash,
             image_mime, recorded_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO NOTHING",
        )
        .bind(&[
            m.id.clone().into(),
            m.memory_type.as_str().into(),
            m.content.clone().into(),
            m.summary.clone().into(),
            m.created_at.to_rfc3339().into(),
            m.last_accessed.to_rfc3339().into(),
            (m.access_count as i32).into(),
            m.strength.into(),
            m.stability.into(),
            match &m.entity {
                Some(e) => e.clone().into(),
                None => worker::wasm_bindgen::JsValue::NULL,
            },
            tags_json.into(),
            match &m.image_hash {
                Some(h) => h.clone().into(),
                None => worker::wasm_bindgen::JsValue::NULL,
            },
            match &m.image_mime {
                Some(t) => t.clone().into(),
                None => worker::wasm_bindgen::JsValue::NULL,
            },
            match &m.recorded_by {
                Some(r) => r.clone().into(),
                None => worker::wasm_bindgen::JsValue::NULL,
            },
        ])?
        .run()
        .await?;

    // workers-rs's `run()` returns Ok as long as the JS Promise resolves.
    // D1 can resolve a Promise with `{success: false}` for constraint /
    // type-coercion failures — those need to be surfaced explicitly or
    // they show up as "200 OK from /admin/import but no rows in D1."
    // PK conflicts no longer reach this check (handled at SQL level via
    // ON CONFLICT DO NOTHING), so this is now purely for *other* failures.
    if !result.success() {
        return Err(worker::Error::RustError(format!(
            "D1 INSERT did not succeed for memory {}: {}",
            m.id,
            result.error().unwrap_or_else(|| "no error message".to_string())
        )));
    }
    Ok(())
}

/// Bulk fetch by a list of ids. Used after a Vectorize query returns
/// top-k matches and we need to resolve each id to a full Memory row.
/// Order is NOT preserved relative to the input — callers can re-order
/// by the original score list if needed.
pub async fn get_many(db: &D1Database, ids: &[&str]) -> Result<Vec<Memory>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // D1's parameter binding doesn't expand a Vec into multiple placeholders,
    // so build the IN-clause manually with one `?` per id.
    let placeholders = std::iter::repeat("?").take(ids.len()).collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT id, memory_type, content, summary, created_at, last_accessed,
                access_count, strength, stability, entity, tags, image_hash,
                image_mime, recorded_by
         FROM memories
         WHERE id IN ({})
           AND superseded = 0",
        placeholders
    );
    let bindings: Vec<worker::wasm_bindgen::JsValue> = ids.iter().map(|id| (*id).into()).collect();
    let rows: Vec<MemoryRow> = db
        .prepare(&sql)
        .bind(&bindings)?
        .all()
        .await?
        .results()?;
    Ok(rows.into_iter().map(MemoryRow::into_memory).collect())
}

/// All current orientation memories. Always loaded — the core of identity.
/// Excludes superseded rows: once the orientation distiller can supersede an
/// axis (CLA-125, progression/correction), recall must surface only the current
/// head, never the retired synthesis kept underneath as history.
pub async fn get_orientation(db: &D1Database) -> Result<Vec<Memory>> {
    let rows: Vec<MemoryRow> = db
        .prepare(
            "SELECT id, memory_type, content, summary, created_at, last_accessed,
                    access_count, strength, stability, entity, tags, image_hash,
                    image_mime, recorded_by
             FROM memories
             WHERE memory_type = 'orientation' AND superseded = 0
             ORDER BY created_at ASC",
        )
        .bind(&[])?
        .all()
        .await?
        .results()?;
    Ok(rows.into_iter().map(MemoryRow::into_memory).collect())
}

/// The ACTIVE orientation set — what a fresh instance loads at session-start.
/// `get_orientation` returns every live axis (the judge's view, so it can
/// link/revise/reactivate a dormant one); recall and the hook want only the
/// axes the Whittling left switched on (CLA-126). Migration 0013 defaults
/// `is_active = 1`, so this equals `get_orientation` until the first Whittle.
pub async fn get_active_orientation(db: &D1Database) -> Result<Vec<Memory>> {
    let rows: Vec<MemoryRow> = db
        .prepare(
            "SELECT id, memory_type, content, summary, created_at, last_accessed,
                    access_count, strength, stability, entity, tags, image_hash,
                    image_mime, recorded_by
             FROM memories
             WHERE memory_type = 'orientation' AND superseded = 0 AND is_active = 1
             ORDER BY created_at ASC",
        )
        .bind(&[])?
        .all()
        .await?
        .results()?;
    Ok(rows.into_iter().map(MemoryRow::into_memory).collect())
}

/// Stamp an orientation axis's M — the model-supplied "importance-for-relating"
/// (1–10) read only by the Whittling (CLA-126). No-op when `meaning` is None
/// (the judge omitted it on a revise → keep the existing M; the one-off backfill
/// fills the NULLs on pre-existing axes). Cast to i32 for the JsValue bind —
/// the column is a small INTEGER, never near i32 range.
pub async fn set_meaning(db: &D1Database, id: &str, meaning: Option<i64>) -> Result<()> {
    let Some(m) = meaning else {
        return Ok(());
    };
    db.prepare("UPDATE memories SET meaning = ? WHERE id = ?")
        .bind(&[(m as i32).into(), id.into()])?
        .run()
        .await?;
    Ok(())
}

/// Set the core pin on an orientation axis — the lens/reference re-scope
/// (CLA-126). `true` pins it as a lens (never deactivated, higher size ceiling);
/// `false` demotes it to reference. Reversible; the backup holds the prior flags.
pub async fn set_core(db: &D1Database, id: &str, core: bool) -> Result<()> {
    let v: i32 = if core { 1 } else { 0 };
    db.prepare("UPDATE memories SET core = ? WHERE id = ?")
        .bind(&[v.into(), id.into()])?
        .run()
        .await?;
    Ok(())
}

// ── Stage C: the Whittling (CLA-126) ─────────────────────────────────────────
// The pure-query maintenance cut. These two primitives are the only DB the
// Whittle touches: read every live axis with its R/F/M ingredients, then apply
// the active/dormant split atomically. No model is involved — that is the point.

/// One live orientation axis with the raw R/F/M ingredients the Whittle ranks on.
/// `freq` (F) is the inbound semantic_orientation edge count — how many semantics
/// fed this axis. `last_accessed` (R) is the last re-orientation touch. `meaning`
/// (M) is the stored standing-importance, NULL until rated. `content` is carried
/// for the one-off backfill (carve + rate); the Whittle ignores it.
#[derive(serde::Deserialize)]
pub struct OrientRankRow {
    pub id: String,
    pub summary: String,
    pub content: String,
    pub last_accessed: String,
    pub meaning: Option<i64>,
    pub core: i64,
    pub freq: i64,
}

/// Every live orientation axis with its R/F/M ingredients (active or not — the
/// Whittle ranks the whole set to decide who stays active).
pub async fn orientation_ranking_rows(db: &D1Database) -> Result<Vec<OrientRankRow>> {
    let rows: Vec<OrientRankRow> = db
        .prepare(
            "SELECT m.id, m.summary, m.content, m.last_accessed, m.meaning, m.core,
                    (SELECT COUNT(*) FROM consolidation_lineage l
                      WHERE l.parent_id = m.id AND l.edge_type = 'semantic_orientation')
                       AS freq
             FROM memories m
             WHERE m.memory_type = 'orientation' AND m.superseded = 0",
        )
        .bind(&[])?
        .all()
        .await?
        .results()?;
    Ok(rows)
}

/// Apply the Whittle's verdict: switch `active_ids` ON and every other live axis
/// OFF, in ONE atomic batch so recall never sees a moment with orientation
/// blanked. A dormant axis keeps its row and edges — it is reactivated the
/// instant a later Whittle re-ranks it up (deactivate, never delete). Empty
/// `active_ids` means there is no orientation at all → no-op (never blank the set).
pub async fn apply_orientation_activation(db: &D1Database, active_ids: &[String]) -> Result<()> {
    if active_ids.is_empty() {
        return Ok(());
    }
    let deactivate = db
        .prepare(
            "UPDATE memories SET is_active = 0
             WHERE memory_type = 'orientation' AND superseded = 0",
        )
        .bind(&[])?;
    let placeholders = vec!["?"; active_ids.len()].join(", ");
    let activate_sql = format!("UPDATE memories SET is_active = 1 WHERE id IN ({})", placeholders);
    let binds: Vec<worker::wasm_bindgen::JsValue> =
        active_ids.iter().map(|s| s.as_str().into()).collect();
    let activate = db.prepare(activate_sql.as_str()).bind(&binds)?;
    db.batch(vec![deactivate, activate]).await?;
    Ok(())
}

/// N most recently created episodic memories. Used by `recall_orient`
/// (CLA-103) as the "what's been happening lately" half of the
/// conversation-start payload. Ordered by `created_at DESC` rather than
/// `last_accessed DESC` — the intent is chronological-recent ("what
/// happened recently") rather than salience-recent ("what's hot right
/// now"); the salience case is served by `recall_check`.
pub async fn get_recent_episodics(db: &D1Database, limit: usize) -> Result<Vec<Memory>> {
    let rows: Vec<MemoryRow> = db
        .prepare(
            "SELECT id, memory_type, content, summary, created_at, last_accessed,
                    access_count, strength, stability, entity, tags, image_hash,
                    image_mime, recorded_by
             FROM memories
             WHERE memory_type = 'episodic'
             ORDER BY created_at DESC
             LIMIT ?",
        )
        .bind(&[(limit as u32).into()])?
        .all()
        .await?
        .results()?;
    Ok(rows.into_iter().map(MemoryRow::into_memory).collect())
}

/// BM25-ranked full-text search via the memories_fts virtual table
/// (CLA-109). Returns memory IDs in rank order (best match first). The
/// caller passes a pre-built FTS5 MATCH expression — use
/// `crate::hybrid::build_fts_query` to construct one safely from user
/// free-text. An empty / None query short-circuits (caller skips FTS).
///
/// BM25 is FTS5's default ranking function; we don't expose the raw
/// score because RRF fusion uses rank, not score. Returning ids in
/// order is the contract.
pub async fn fts_search(
    db: &D1Database,
    match_expr: &str,
    limit: u32,
) -> Result<Vec<String>> {
    // Trim defensively: build_fts_query (our only current caller) can't
    // produce whitespace-only output, but fts_search is pub and a future
    // caller might. A whitespace-only MATCH expression would error on
    // FTS5's syntax check rather than degenerating cleanly to empty.
    let match_expr = match_expr.trim();
    if match_expr.is_empty() {
        return Ok(Vec::new());
    }
    #[derive(serde::Deserialize)]
    struct FtsHit {
        id: String,
    }
    let rows: Vec<FtsHit> = db
        .prepare(
            "SELECT id FROM memories_fts
             WHERE memories_fts MATCH ?
             ORDER BY rank
             LIMIT ?",
        )
        .bind(&[match_expr.into(), limit.into()])?
        .all()
        .await?
        .results()?;
    Ok(rows.into_iter().map(|r| r.id).collect())
}

/// Memories filtered by entity, ranked by strength.
pub async fn recall_by_entity(
    db: &D1Database,
    entity: &str,
    min_strength: f64,
    limit: usize,
) -> Result<Vec<Memory>> {
    let rows: Vec<MemoryRow> = db
        .prepare(
            "SELECT id, memory_type, content, summary, created_at, last_accessed,
                    access_count, strength, stability, entity, tags, image_hash,
                    image_mime, recorded_by
             FROM memories
             WHERE entity = ?
               AND strength >= ?
             ORDER BY strength DESC
             LIMIT ?",
        )
        .bind(&[entity.into(), min_strength.into(), (limit as u32).into()])?
        .all()
        .await?
        .results()?;
    Ok(rows.into_iter().map(MemoryRow::into_memory).collect())
}

/// Episodics captured but not yet processed by Stage-1 encode (CLA-117).
/// `integrated = 0`, oldest first so the pass clears the backlog FIFO. The
/// trigger-agnostic `encode_one` consumes these whether driven by a queue
/// consumer (per-write) or a nightly cron batch.
pub async fn find_unintegrated_episodics(db: &D1Database, limit: u32) -> Result<Vec<Memory>> {
    let rows: Vec<MemoryRow> = db
        .prepare(
            "SELECT id, memory_type, content, summary, created_at, last_accessed,
                    access_count, strength, stability, entity, tags, image_hash,
                    image_mime, recorded_by
             FROM memories
             WHERE memory_type = 'episodic' AND integrated = 0
             ORDER BY created_at ASC
             LIMIT ?",
        )
        .bind(&[limit.into()])?
        .all()
        .await?
        .results()?;
    Ok(rows.into_iter().map(MemoryRow::into_memory).collect())
}

/// Semantics that gained an inbound episodic edge since `since` (RFC3339) —
/// i.e. subjects that came back onto the radar in that window (CLA-125). The
/// orientation distiller's trigger: a fresh edge onto an *old* semantic counts,
/// which is the whole point — currency is edge-recency, not the semantic's age.
/// Excludes superseded; newest-created first; capped by `limit`.
pub async fn find_semantics_edged_since(
    db: &D1Database,
    since: &str,
    limit: u32,
) -> Result<Vec<Memory>> {
    let rows: Vec<MemoryRow> = db
        .prepare(
            "SELECT id, memory_type, content, summary, created_at, last_accessed,
                    access_count, strength, stability, entity, tags, image_hash,
                    image_mime, recorded_by
             FROM memories
             WHERE memory_type = 'semantic' AND superseded = 0
               AND id IN (
                   SELECT DISTINCT parent_id FROM consolidation_lineage
                   WHERE edge_type = 'episodic_semantic' AND created_at >= ?
               )
             ORDER BY created_at DESC
             LIMIT ?",
        )
        .bind(&[since.into(), limit.into()])?
        .all()
        .await?
        .results()?;
    Ok(rows.into_iter().map(MemoryRow::into_memory).collect())
}

/// Mark an episodic as processed by Stage-1 encode so the next pass skips it.
pub async fn mark_integrated(db: &D1Database, id: &str) -> Result<()> {
    db.prepare("UPDATE memories SET integrated = 1 WHERE id = ?")
        .bind(&[id.into()])?
        .run()
        .await?;
    Ok(())
}

// ── Encode batch ledger + failure backstop (CLA-132) ────────────────────────
// The async encode pipeline submits each capture as an Anthropic Message Batch
// and dispatches the result from a delayed poll. `encode_batches` is the
// in-flight ledger; `encode_failures` is the queryable record of episodics that
// never landed (they stay unintegrated; `/admin/encode-run` re-submits them).

#[derive(serde::Deserialize)]
struct EncodeBatchSqlRow {
    batch_id: String,
    episodic_ids: String,
    poll_count: i64,
}

/// One in-flight encode batch: the Anthropic batch id, the episodic ids it
/// carries (custom_id = episodic_id), and how many times it's been polled.
pub struct EncodeBatchRow {
    pub batch_id: String,
    pub episodic_ids: Vec<String>,
    pub poll_count: i64,
}

/// Record a freshly-submitted batch as in-flight.
pub async fn insert_encode_batch(
    db: &D1Database,
    batch_id: &str,
    episodic_ids: &[String],
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let ids_json = serde_json::to_string(episodic_ids).unwrap_or_else(|_| "[]".to_string());
    db.prepare(
        "INSERT INTO encode_batches (batch_id, status, episodic_ids, submitted_at, poll_count)
         VALUES (?, 'in_progress', ?, ?, 0)",
    )
    .bind(&[batch_id.into(), ids_json.into(), now.into()])?
    .run()
    .await?;
    Ok(())
}

/// Every batch still awaiting results, oldest first.
pub async fn in_progress_encode_batches(db: &D1Database) -> Result<Vec<EncodeBatchRow>> {
    let rows: Vec<EncodeBatchSqlRow> = db
        .prepare(
            "SELECT batch_id, episodic_ids, poll_count
             FROM encode_batches WHERE status = 'in_progress'
             ORDER BY submitted_at ASC",
        )
        .all()
        .await?
        .results()?;
    Ok(rows
        .into_iter()
        .map(|r| EncodeBatchRow {
            batch_id: r.batch_id,
            episodic_ids: serde_json::from_str(&r.episodic_ids).unwrap_or_default(),
            poll_count: r.poll_count,
        })
        .collect())
}

/// Move a batch to a terminal state ('dispatched' | 'failed'), stamping the poll.
pub async fn set_encode_batch_status(db: &D1Database, batch_id: &str, status: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    db.prepare(
        "UPDATE encode_batches SET status = ?, polled_at = ?, poll_count = poll_count + 1
         WHERE batch_id = ?",
    )
    .bind(&[status.into(), now.into(), batch_id.into()])?
    .run()
    .await?;
    Ok(())
}

/// Note a poll of a batch that's still open (bumps the counter for give-up logic).
pub async fn bump_encode_batch_poll(db: &D1Database, batch_id: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    db.prepare(
        "UPDATE encode_batches SET polled_at = ?, poll_count = poll_count + 1
         WHERE batch_id = ?",
    )
    .bind(&[now.into(), batch_id.into()])?
    .run()
    .await?;
    Ok(())
}

/// Record an episodic whose encode failed to land — the queryable backstop
/// (CLA-132). The episodic stays unintegrated; `/admin/encode-run` re-submits it.
pub async fn record_encode_failure(
    db: &D1Database,
    episodic_id: &str,
    batch_id: &str,
    reason: &str,
) -> Result<()> {
    let failure_id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    db.prepare(
        "INSERT INTO encode_failures (failure_id, episodic_id, batch_id, reason, failed_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&[
        failure_id.into(),
        episodic_id.into(),
        batch_id.into(),
        reason.into(),
        now.into(),
    ])?
    .run()
    .await?;
    Ok(())
}

// ── Encode diagnostics (CLA-132): forensic capture of each judge call ────────
// One row per judge call, so an async first-submission whiff leaves its own
// post-mortem. Sentinels instead of NULLs (-1 / "") keep the bind trivial — it's
// a debug table, read by the admin endpoint, never by the hot path.

/// A single judge-call autopsy. All concrete (sentinels, not NULLs).
pub struct EncodeDiagnostic<'a> {
    pub episodic_id: &'a str,
    pub source: &'a str,
    pub candidate_count: i64,
    pub content_chars: i64,
    pub http_status: i64,
    pub stop_reason: String,
    pub usage_in: i64,
    pub usage_out: i64,
    pub has_tool_use: bool,
    pub content_types: String,
    pub decision_count: i64,
    pub note: String,
}

/// Record one judge-call autopsy. Best-effort at the call site (`let _`).
pub async fn insert_encode_diagnostic(db: &D1Database, d: &EncodeDiagnostic<'_>) -> Result<()> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    db.prepare(
        "INSERT INTO encode_diagnostics
            (diag_id, episodic_id, source, candidate_count, content_chars, http_status,
             stop_reason, usage_in, usage_out, has_tool_use, content_types,
             decision_count, note, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&[
        id.into(),
        d.episodic_id.into(),
        d.source.into(),
        (d.candidate_count as i32).into(),
        (d.content_chars as i32).into(),
        (d.http_status as i32).into(),
        d.stop_reason.as_str().into(),
        (d.usage_in as i32).into(),
        (d.usage_out as i32).into(),
        (if d.has_tool_use { 1i32 } else { 0i32 }).into(),
        d.content_types.as_str().into(),
        (d.decision_count as i32).into(),
        d.note.as_str().into(),
        now.into(),
    ])?
    .run()
    .await?;
    Ok(())
}

/// The N most recent autopsies, newest first — read by `/admin/encode-diagnostics`.
#[derive(serde::Deserialize)]
pub struct EncodeDiagRow {
    pub episodic_id: String,
    pub source: Option<String>,
    pub candidate_count: Option<i64>,
    pub content_chars: Option<i64>,
    pub http_status: Option<i64>,
    pub stop_reason: Option<String>,
    pub usage_in: Option<i64>,
    pub usage_out: Option<i64>,
    pub has_tool_use: Option<i64>,
    pub content_types: Option<String>,
    pub decision_count: Option<i64>,
    pub note: Option<String>,
    pub created_at: String,
}

pub async fn recent_encode_diagnostics(db: &D1Database, limit: u32) -> Result<Vec<EncodeDiagRow>> {
    let rows: Vec<EncodeDiagRow> = db
        .prepare(
            "SELECT episodic_id, source, candidate_count, content_chars, http_status,
                    stop_reason, usage_in, usage_out, has_tool_use, content_types,
                    decision_count, note, created_at
             FROM encode_diagnostics ORDER BY created_at DESC LIMIT ?",
        )
        .bind(&[limit.into()])?
        .all()
        .await?
        .results()?;
    Ok(rows)
}

/// Write one provenance edge with explicit weight + type (CLA-117). Used for
/// the encapsulated/link-only case (an episodic folded into an existing
/// semantic with no change to it), and as the typed/weighted edge primitive
/// generally. `edge_type` is functionally determined by the endpoints and
/// validated by the caller. INSERT OR IGNORE so a re-run can't duplicate an
/// edge — the PK is (parent_id, source_id).
pub async fn add_lineage_edge(
    db: &D1Database,
    parent_id: &str,
    source_id: &str,
    weight: f64,
    edge_type: &str,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    db.prepare(
        "INSERT OR IGNORE INTO consolidation_lineage
            (parent_id, source_id, created_at, weight, edge_type)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&[
        parent_id.into(),
        source_id.into(),
        now.into(),
        weight.into(),
        edge_type.into(),
    ])?
    .run()
    .await?;
    Ok(())
}

/// Migration pre-step (CLA-122): episodics already cited as a `source_id` in
/// `consolidation_lineage` were consolidated under old REM, and `0008`
/// backfilled their edges — so they are already "encoded" in the new model.
/// Mark them integrated so the Stage-1 pass only touches the un-encoded
/// backlog (reflections + whatever old REM skipped). Returns how many were
/// marked.
pub async fn mark_consolidated_episodics_integrated(db: &D1Database) -> Result<u64> {
    #[derive(serde::Deserialize)]
    struct CountRow {
        n: u64,
    }
    let row = db
        .prepare(
            "SELECT COUNT(*) AS n FROM memories
             WHERE memory_type = 'episodic' AND integrated = 0
               AND id IN (SELECT source_id FROM consolidation_lineage)",
        )
        .first::<CountRow>(None)
        .await?;
    let n = row.map(|r| r.n).unwrap_or(0);

    db.prepare(
        "UPDATE memories SET integrated = 1
         WHERE memory_type = 'episodic' AND integrated = 0
           AND id IN (SELECT source_id FROM consolidation_lineage)",
    )
    .run()
    .await?;

    Ok(n)
}

/// Flag a semantic as superseded by a newer one (CLA-117). The old view is
/// kept in the store for completeness but excluded from tool surfacing (via
/// the `superseded = 0` filter in `get_many`) — functionally the same as
/// forgotten. `new_id` is the semantic that replaced it.
pub async fn mark_superseded(db: &D1Database, old_id: &str, new_id: &str) -> Result<()> {
    db.prepare("UPDATE memories SET superseded = 1, superseded_by = ? WHERE id = ?")
        .bind(&[new_id.into(), old_id.into()])?
        .run()
        .await?;
    Ok(())
}

/// N most recently created semantic memories. Used by the dialectic
/// (CLA-95) as its Stage 1 candidate selection — most-recent semantics
/// are the freshest candidates for scrutiny and the highest-leverage
/// place to catch inflation before it consolidates into the system's
/// stable understanding. Stage 3 will replace this with a richer
/// selection (skip recently-reviewed, weight by access count, etc.).
/// Recent semantic memories *excluding* those the dialectic has judged
/// within the last `cooldown_days`. Used by Stage 1 candidate selection
/// to prevent re-litigation of recently-evaluated memories (CLA-101).
///
/// A memory is excluded if any row in `dialectic_decisions` references
/// it with a `created_at` more recent than `now - cooldown_days`. The
/// cooldown gates on *any* decision regardless of action — including
/// well_calibrated short-circuits — so the dialectic spreads its
/// attention across the full semantic pool over the cooldown window
/// instead of re-judging the same N most-recent memories every night.
///
/// Why gate on `dialectic_decisions.created_at` rather than
/// `memory_reframes.reframed_at`: in dry-run mode the dispatcher never
/// writes to `memory_reframes`, so a `memory_reframes`-based gate is
/// silent until live cutover and the same memories keep getting
/// re-judged every night during burn-in. Decision-time is the event
/// we want to cool down on (Haiku has run, dialogue has happened, a
/// proposal exists) — not the eventual application of that proposal.
///
/// `datetime(d.last_decided_at)` normalises the RFC 3339 string we
/// store into SQLite's native datetime format so the comparison against
/// `datetime('now', ...)` is structural, not lexicographic. (Without it
/// the `T` separator and timezone suffix in RFC 3339 don't compare
/// cleanly against SQLite's default `YYYY-MM-DD HH:MM:SS`.)
pub async fn recent_semantics_not_recently_judged(
    db: &D1Database,
    limit: usize,
    cooldown_days: u32,
) -> Result<Vec<Memory>> {
    let rows: Vec<MemoryRow> = db
        .prepare(
            "SELECT m.id, m.memory_type, m.content, m.summary, m.created_at,
                    m.last_accessed, m.access_count, m.strength, m.stability,
                    m.entity, m.tags, m.image_hash, m.image_mime, m.recorded_by
             FROM memories m
             LEFT JOIN (
                 SELECT memory_id, MAX(created_at) AS last_decided_at
                 FROM dialectic_decisions
                 GROUP BY memory_id
             ) d ON m.id = d.memory_id
             WHERE m.memory_type = 'semantic'
               AND (d.last_decided_at IS NULL
                    OR datetime(d.last_decided_at) < datetime('now', '-' || ? || ' days'))
             ORDER BY m.created_at DESC
             LIMIT ?",
        )
        .bind(&[cooldown_days.into(), (limit as u32).into()])?
        .all()
        .await?
        .results()?;
    Ok(rows.into_iter().map(MemoryRow::into_memory).collect())
}

/// One row of the semantic layer for the cluster-preview diagnostic
/// (CLA-134): id + summary + strength. We don't carry full content or the
/// vector here — the vector is fetched from Vectorize by id, and the summary
/// is enough to eyeball whether a proximity cluster is a genuine near-dupe.
pub struct SemanticBrief {
    pub id: String,
    pub summary: String,
    pub strength: f64,
}

/// Every live (non-superseded) semantic's id + summary + strength, oldest
/// first. Read-only enumeration of the semantic layer for cluster-preview —
/// the calibration pass that tunes the consolidation similarity threshold
/// before any Haiku judgment or writes exist. `COALESCE(summary, '')` guards
/// the handful of legacy rows that predate the summary column.
pub async fn all_live_semantics(db: &D1Database) -> Result<Vec<SemanticBrief>> {
    #[derive(serde::Deserialize)]
    struct Row {
        id: String,
        summary: String,
        strength: f64,
    }
    let rows: Vec<Row> = db
        .prepare(
            "SELECT id, COALESCE(summary, '') AS summary, strength
             FROM memories
             WHERE memory_type = 'semantic' AND superseded = 0
             ORDER BY created_at ASC",
        )
        .all()
        .await?
        .results()?;
    Ok(rows
        .into_iter()
        .map(|r| SemanticBrief {
            id: r.id,
            summary: r.summary,
            strength: r.strength,
        })
        .collect())
}

/// Resolve an 8-char prefix (as shown in recall output) to a full UUID.
/// Returns `Ok(None)` if no match or if the prefix is ambiguous.
pub async fn find_by_prefix(db: &D1Database, prefix: &str) -> Result<Option<Memory>> {
    if prefix.len() >= 36 {
        return get(db, prefix).await;
    }
    let pattern = format!("{}%", prefix);
    let rows: Vec<MemoryRow> = db
        .prepare(
            "SELECT id, memory_type, content, summary, created_at, last_accessed,
                    access_count, strength, stability, entity, tags, image_hash,
                    image_mime, recorded_by
             FROM memories
             WHERE id LIKE ?
             LIMIT 2",
        )
        .bind(&[pattern.into()])?
        .all()
        .await?
        .results()?;
    if rows.len() != 1 {
        // 0 = not found, >1 = ambiguous prefix
        return Ok(None);
    }
    Ok(Some(rows.into_iter().next().unwrap().into_memory()))
}

/// Counts grouped by memory type. Returns (episodic, semantic, orientation).
pub async fn count_by_type(db: &D1Database) -> Result<(u64, u64, u64)> {
    #[derive(serde::Deserialize)]
    struct CountRow {
        n: u64,
    }
    async fn count_where(db: &D1Database, kind: &str) -> Result<u64> {
        let row = db
            .prepare("SELECT COUNT(*) AS n FROM memories WHERE memory_type = ?")
            .bind(&[kind.into()])?
            .first::<CountRow>(None)
            .await?;
        Ok(row.map(|r| r.n).unwrap_or(0))
    }
    let episodic = count_where(db, "episodic").await?;
    let semantic = count_where(db, "semantic").await?;
    let orientation = count_where(db, "orientation").await?;
    Ok((episodic, semantic, orientation))
}

/// Reinforce a memory on recall: bump access_count, refresh last_accessed,
/// boost stability (Hebbian — recalled memories decay slower).
pub async fn touch(db: &D1Database, id: &str) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    // Stability boost: 1.4x per access, matching the native store's formula
    // (computed by SQL since we don't want a SELECT-then-UPDATE round trip).
    db.prepare(
        "UPDATE memories
         SET access_count = access_count + 1,
             last_accessed = ?,
             stability = stability * 1.4,
             strength = 1.0
         WHERE id = ?",
    )
    .bind(&[now.into(), id.into()])?
    .run()
    .await?;
    Ok(())
}

/// Update a memory's content + summary, preserving the original first.
///
/// This is the only in-place-destructive path in the store, so the
/// overwrite is bound to its own undo: an atomic D1 batch writes a
/// `memory_revisions` audit row (carrying old_content/old_summary) in the
/// SAME transaction as the UPDATE. Either both land or neither — the
/// original can never be lost to a mis-targeted reframe (CLA-130). This
/// generalises the dialectic's `memory_reframes` safety net to every
/// caller, so none of them has to remember to preserve.
///
/// `source` names the caller ('encode' | 'orient' | 'reframe-tool' |
/// 'reflect-tool') and `rationale` carries the judge's stated reason —
/// the relationship gate — so a mis-fire can be read back against the
/// content it touched. `None`/empty for the conscious MCP paths.
///
/// Does NOT touch the embedding — Vectorize upsert is the caller's job
/// since we don't generate embeddings inside the store.
pub async fn reframe(
    db: &D1Database,
    id: &str,
    source: &str,
    rationale: Option<&str>,
    new_content: &str,
    new_summary: &str,
) -> Result<bool> {
    // Preserve the original before we overwrite it. If the row is already
    // gone, there is nothing to reframe (and nothing to lose) — no-op.
    let Some(original) = get(db, id).await? else {
        return Ok(false);
    };

    let now = chrono::Utc::now().to_rfc3339();
    let revision_id = Uuid::new_v4().to_string();

    let update_stmt = db
        .prepare(
            "UPDATE memories
             SET content = ?, summary = ?, last_accessed = ?
             WHERE id = ?",
        )
        .bind(&[
            new_content.into(),
            new_summary.into(),
            now.clone().into(),
            id.into(),
        ])?;

    let audit_stmt = db
        .prepare(
            "INSERT INTO memory_revisions
                (revision_id, memory_id, source, rationale,
                 old_content, old_summary, new_content, new_summary, revised_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&[
            revision_id.into(),
            id.into(),
            source.into(),
            rationale.unwrap_or("").into(),
            original.content.into(),
            original.summary.into(),
            new_content.into(),
            new_summary.into(),
            now.into(),
        ])?;

    // Atomic: the destructive UPDATE can never commit without its undo row.
    // On batch failure neither statement lands and the memory is untouched.
    db.batch(vec![update_stmt, audit_stmt]).await?;
    Ok(true)
}

/// Forget a memory — DELETE plus a tombstone row. Orientation memories
/// cannot be forgotten (identity is non-negotiable). Returns whether a
/// row was actually removed.
pub async fn forget(db: &D1Database, id: &str) -> Result<bool> {
    // First check it's not orientation.
    let maybe = get(db, id).await?;
    let Some(memory) = maybe else {
        return Ok(false);
    };
    if memory.memory_type == MemoryType::Orientation {
        return Ok(false);
    }

    // Clean co-activations first (FK references memories).
    db.prepare("DELETE FROM co_activations WHERE memory_a = ? OR memory_b = ?")
        .bind(&[id.into(), id.into()])?
        .run()
        .await?;

    let deleted = db
        .prepare("DELETE FROM memories WHERE id = ?")
        .bind(&[id.into()])?
        .run()
        .await?;
    let changes = deleted.meta().ok().flatten().and_then(|m| m.changes).unwrap_or(0);
    if changes > 0 {
        let now = chrono::Utc::now().to_rfc3339();
        db.prepare(
            "INSERT OR IGNORE INTO tombstones (memory_id, forgotten_at) VALUES (?, ?)",
        )
        .bind(&[id.into(), now.into()])?
        .run()
        .await?;
    }
    Ok(changes > 0)
}

/// Apply Ebbinghaus-style decay to all non-orientation memories. Decreases
/// `strength` based on time since last_accessed and the per-memory
/// `stability` parameter.
///
/// Implementation note: SQLite (and D1) doesn't have a native exp function
/// in its default builds, so we compute the decay client-side by fetching
/// candidate rows, computing new strengths in Rust, and writing back via
/// a batched UPDATE. For now we do per-row updates in a loop — fine for
/// the few-hundred-memory scale; can batch later if needed.
pub async fn apply_decay(db: &D1Database) -> Result<usize> {
    #[derive(serde::Deserialize)]
    struct DecayRow {
        id: String,
        last_accessed: String,
        stability: f64,
    }

    let rows: Vec<DecayRow> = db
        .prepare(
            "SELECT id, last_accessed, stability
             FROM memories
             WHERE memory_type != 'orientation'",
        )
        .bind(&[])?
        .all()
        .await?
        .results()?;

    let now = chrono::Utc::now();
    let mut updated = 0usize;
    for row in rows {
        let Ok(last_accessed) = chrono::DateTime::parse_from_rfc3339(&row.last_accessed) else {
            continue;
        };
        let days = (now - last_accessed.with_timezone(&chrono::Utc)).num_seconds() as f64
            / 86_400.0;
        let new_strength = (-days / row.stability).exp().clamp(0.0, 1.0);
        db.prepare("UPDATE memories SET strength = ? WHERE id = ?")
            .bind(&[new_strength.into(), row.id.into()])?
            .run()
            .await?;
        updated += 1;
    }
    Ok(updated)
}

/// Record that a set of memory IDs were surfaced together — Hebbian
/// reinforcement of their pairwise bonds. Updates co_activations with
/// INSERT … ON CONFLICT … DO UPDATE for atomic increment.
pub async fn record_co_activation(db: &D1Database, ids: &[&str]) -> Result<()> {
    if ids.len() < 2 {
        return Ok(());
    }
    let now = chrono::Utc::now().to_rfc3339();
    for i in 0..ids.len() {
        for j in (i + 1)..ids.len() {
            // Canonical ordering so (A, B) and (B, A) collapse to one row.
            let (a, b) = if ids[i] < ids[j] {
                (ids[i], ids[j])
            } else {
                (ids[j], ids[i])
            };
            db.prepare(
                "INSERT INTO co_activations (memory_a, memory_b, count, last_co_activated)
                 VALUES (?, ?, 1, ?)
                 ON CONFLICT(memory_a, memory_b)
                 DO UPDATE SET count = count + 1, last_co_activated = excluded.last_co_activated",
            )
            .bind(&[a.into(), b.into(), now.clone().into()])?
            .run()
            .await?;
        }
    }
    Ok(())
}

/// Map an image MIME type to a filesystem-style extension. Mirrors
/// store::mime_to_ext on the native side so R2 keys and local
/// filesystem paths line up exactly — useful for future export.
fn mime_to_ext(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        _ => "bin",
    }
}

/// Content-addressed image upload. SHA-256 the bytes, write to R2 under
/// `{hash}.{ext}` (no-op if the key already exists), return the hex hash.
/// Mirrors store::store_image on the native side.
pub async fn store_image_to_r2(
    bucket: &Bucket,
    bytes: Vec<u8>,
    mime: &str,
) -> Result<String> {
    let hash = hex::encode(Sha256::digest(&bytes));
    let key = format!("{}.{}", hash, mime_to_ext(mime));
    // Dedup: HEAD the key first, only write if missing. R2 PUT is
    // idempotent (same key + same body) but the HEAD saves the bytes
    // over the wire when we already have it.
    if bucket.head(&key).await?.is_none() {
        bucket.put(&key, bytes).execute().await?;
    }
    Ok(hash)
}

/// Fetch image bytes from R2 by hash + mime. Returns None when the key
/// isn't present (e.g. orphaned memory row, or hash mismatch).
pub async fn read_image_from_r2(
    bucket: &Bucket,
    hash: &str,
    mime: &str,
) -> Result<Option<Vec<u8>>> {
    let key = format!("{}.{}", hash, mime_to_ext(mime));
    let Some(obj) = bucket.get(&key).execute().await? else {
        return Ok(None);
    };
    let Some(body) = obj.body() else {
        return Ok(None);
    };
    Ok(Some(body.bytes().await?))
}

/// Create a memory with an associated image. Bytes go to R2 (content-
/// addressed), the resulting hash goes into the memory row. Same
/// provenance rules as `create_memory_with_provenance` —
/// `recorded_by` is server-controlled.
pub async fn create_memory_with_image_and_provenance(
    db: &D1Database,
    bucket: &Bucket,
    memory_type: MemoryType,
    content: String,
    summary: String,
    entity: Option<String>,
    tags: Vec<String>,
    image_bytes: Vec<u8>,
    image_mime: String,
    recorded_by: Option<String>,
) -> Result<Memory> {
    let hash = store_image_to_r2(bucket, image_bytes, &image_mime).await?;
    let now = Utc::now();
    let id = Uuid::new_v4().to_string();
    let tags_json = serde_json::to_string(&tags).unwrap_or_else(|_| "[]".to_string());
    let stability = memory_type.base_stability();

    db.prepare(
        "INSERT INTO memories
            (id, memory_type, content, summary, created_at, last_accessed,
             access_count, strength, stability, entity, tags, image_hash,
             image_mime, recorded_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&[
        id.clone().into(),
        memory_type.as_str().into(),
        content.clone().into(),
        summary.clone().into(),
        now.to_rfc3339().into(),
        now.to_rfc3339().into(),
        0i32.into(),
        1.0_f64.into(),
        stability.into(),
        match &entity {
            Some(e) => e.clone().into(),
            None => worker::wasm_bindgen::JsValue::NULL,
        },
        tags_json.into(),
        hash.clone().into(),
        image_mime.clone().into(),
        match &recorded_by {
            Some(r) => r.clone().into(),
            None => worker::wasm_bindgen::JsValue::NULL,
        },
    ])?
    .run()
    .await?;

    Ok(Memory {
        id,
        memory_type,
        content,
        summary,
        created_at: now,
        last_accessed: now,
        access_count: 0,
        strength: 1.0,
        stability,
        entity,
        tags,
        embedding: None,
        image_hash: Some(hash),
        image_mime: Some(image_mime),
        recorded_by,
    })
}

/// **LEGACY — DO NOT USE FROM NEW CODE.**
///
/// This is the pre-CLA-87 merge-and-replace consolidation pattern, where
/// two parent memories were folded into one synthesized semantic. It's
/// kept temporarily for native-side compatibility and to preserve the
/// historical contract, but the worker-side REM consolidator (CLA-87)
/// uses the additive pattern instead — see `create_semantic_with_lineage`
/// and `update_semantic_with_lineage`, which preserve the source
/// episodics rather than destroying them.
#[allow(dead_code)]
pub async fn consolidate(
    db: &D1Database,
    parent_a: &Memory,
    parent_b: &Memory,
    merged_content: String,
    merged_summary: String,
) -> Result<Memory> {
    let entity = match (&parent_a.entity, &parent_b.entity) {
        (Some(a), Some(b)) if a == b => Some(a.clone()),
        (Some(a), None) => Some(a.clone()),
        (None, Some(b)) => Some(b.clone()),
        _ => None,
    };
    let mut tag_set: std::collections::BTreeSet<String> = parent_a.tags.iter().cloned().collect();
    tag_set.extend(parent_b.tags.iter().cloned());
    let tags: Vec<String> = tag_set.into_iter().collect();

    let combined_access = parent_a.access_count + parent_b.access_count;
    let boosted_stability =
        MemoryType::Semantic.base_stability() * 1.4_f64.powi(combined_access as i32);

    let memory = create_memory_with_provenance(
        db,
        MemoryType::Semantic,
        merged_content,
        merged_summary,
        entity,
        tags,
        Some("rem".to_string()),
    )
    .await?;

    // Bump stability beyond the base, reflecting the parents' reinforcement.
    db.prepare("UPDATE memories SET stability = ? WHERE id = ?")
        .bind(&[boosted_stability.into(), memory.id.clone().into()])?
        .run()
        .await?;

    Ok(Memory {
        stability: boosted_stability,
        ..memory
    })
}

// ────────────────────────────────────────────────────────────────────────
// CLA-87 — Additive consolidation primitives
//
// The post-CLA-87 consolidation model preserves source episodics rather
// than destroying them. REM produces semantic memories that *cite* their
// source episodics via the `consolidation_lineage` junction table; the
// episodics themselves remain in the store and continue to be retrievable
// independently. MMR-rerank at recall time handles the dilution that the
// old merge-and-replace pattern was trying to solve at consolidation time.
// ────────────────────────────────────────────────────────────────────────

/// Find candidate pairs for REM consolidation. Filters applied:
///
///   * `count >= min_count` — only pairs with non-trivial Hebbian bond
///   * neither memory is an orientation (orientations don't consolidate)
///   * neither memory is already a `source_id` in `consolidation_lineage`
///     (it's already been folded into some existing semantic)
///   * neither memory is already a `parent_id` in `consolidation_lineage`
///     (don't re-consolidate consolidated semantics with their siblings)
///
/// Returns `(memory_a_id, memory_b_id, count)` tuples, ordered by count
/// descending. The REM worker then runs union-find over these pairs to
/// build connected components (clusters) for the Haiku call.
pub async fn find_consolidation_pairs(
    db: &D1Database,
    min_count: u32,
    limit: u32,
) -> Result<Vec<(String, String, u32)>> {
    #[derive(serde::Deserialize)]
    struct PairRow {
        memory_a: String,
        memory_b: String,
        count: u32,
    }

    let rows: Vec<PairRow> = db
        .prepare(
            "SELECT memory_a, memory_b, count
             FROM co_activations
             WHERE count >= ?
               AND memory_a IN (SELECT id FROM memories WHERE memory_type != 'orientation')
               AND memory_b IN (SELECT id FROM memories WHERE memory_type != 'orientation')
               AND memory_a NOT IN (SELECT source_id FROM consolidation_lineage)
               AND memory_b NOT IN (SELECT source_id FROM consolidation_lineage)
               AND memory_a NOT IN (SELECT parent_id FROM consolidation_lineage)
               AND memory_b NOT IN (SELECT parent_id FROM consolidation_lineage)
             ORDER BY count DESC
             LIMIT ?",
        )
        .bind(&[min_count.into(), limit.into()])?
        .all()
        .await?
        .results()?;

    Ok(rows.into_iter()
        .map(|r| (r.memory_a, r.memory_b, r.count))
        .collect())
}

/// Create a new semantic memory consolidated from one or more source
/// episodics. Inserts the memory row and one `consolidation_lineage`
/// row per source. The source episodics are NOT modified.
///
/// `recorded_by` is set to `"rem-worker"` automatically — the
/// consolidator's provenance, which lets audit queries answer
/// "what did REM write last night?" trivially.
pub async fn create_semantic_with_lineage(
    db: &D1Database,
    content: String,
    summary: String,
    entity: Option<String>,
    tags: Vec<String>,
    source_ids: &[&str],
) -> Result<Memory> {
    let memory = create_memory_with_provenance(
        db,
        MemoryType::Semantic,
        content,
        summary,
        entity,
        tags,
        Some("rem-worker".to_string()),
    )
    .await?;

    let now = chrono::Utc::now().to_rfc3339();
    for source_id in source_ids {
        db.prepare(
            "INSERT OR IGNORE INTO consolidation_lineage
                (parent_id, source_id, created_at)
             VALUES (?, ?, ?)",
        )
        .bind(&[
            memory.id.clone().into(),
            (*source_id).into(),
            now.clone().into(),
        ])?
        .run()
        .await?;
    }

    Ok(memory)
}

/// Update an existing semantic memory's content + summary AND add new
/// source episodics to its lineage. Used by REM when Haiku's decision
/// is `append` or `revise` — same SQL shape, the prompt-level semantic
/// difference lives upstream in the worker_rem dispatch.
///
/// Returns true if a row was updated. The memory's `recorded_by` is
/// preserved (was set when the semantic was first created); the
/// "REM updated this on date X" fact lives in the audit log instead.
pub async fn update_semantic_with_lineage(
    db: &D1Database,
    id: &str,
    new_content: &str,
    new_summary: &str,
    additional_source_ids: &[&str],
) -> Result<bool> {
    let now = chrono::Utc::now().to_rfc3339();
    let result = db
        .prepare(
            "UPDATE memories
             SET content = ?, summary = ?, last_accessed = ?
             WHERE id = ? AND memory_type = 'semantic'",
        )
        .bind(&[
            new_content.into(),
            new_summary.into(),
            now.clone().into(),
            id.into(),
        ])?
        .run()
        .await?;

    let updated = result
        .meta()
        .ok()
        .flatten()
        .and_then(|m| m.changes)
        .unwrap_or(0)
        > 0;
    if !updated {
        return Ok(false);
    }

    for source_id in additional_source_ids {
        db.prepare(
            "INSERT OR IGNORE INTO consolidation_lineage
                (parent_id, source_id, created_at)
             VALUES (?, ?, ?)",
        )
        .bind(&[id.into(), (*source_id).into(), now.clone().into()])?
        .run()
        .await?;
    }

    Ok(true)
}

// ──── Cluster decisions cache (CLA-94) ──────────────────────────────────

/// A cached cluster decision — same UUID set produced this action last
/// time REM judged it. Looked up by `cluster_hash` (SHA-256 of sorted
/// member UUIDs, computed in `worker_rem::compute_cluster_hash`) to
/// avoid re-invoking Haiku on stable clusters.
#[derive(Debug, Clone)]
pub struct ClusterDecision {
    pub cluster_hash: String,
    pub members_json: String,
    pub last_action: String,
    pub result_memory_id: Option<String>,
    pub first_decided_at: DateTime<Utc>,
    pub last_decided_at: DateTime<Utc>,
    pub decision_count: u32,
}

#[derive(Debug, serde::Deserialize)]
struct ClusterDecisionRow {
    cluster_hash: String,
    members_json: String,
    last_action: String,
    result_memory_id: Option<String>,
    first_decided_at: String,
    last_decided_at: String,
    decision_count: u32,
}

impl ClusterDecisionRow {
    fn into_decision(self) -> ClusterDecision {
        let first_decided_at = DateTime::parse_from_rfc3339(&self.first_decided_at)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        let last_decided_at = DateTime::parse_from_rfc3339(&self.last_decided_at)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        ClusterDecision {
            cluster_hash: self.cluster_hash,
            members_json: self.members_json,
            last_action: self.last_action,
            result_memory_id: self.result_memory_id,
            first_decided_at,
            last_decided_at,
            decision_count: self.decision_count,
        }
    }
}

/// Fetch a cached decision for a cluster, or `None` if this cluster
/// hasn't been judged before. Cache hit means REM can skip the Haiku
/// call and reuse the prior decision.
pub async fn fetch_cluster_decision(
    db: &D1Database,
    cluster_hash: &str,
) -> Result<Option<ClusterDecision>> {
    let row = db
        .prepare(
            "SELECT cluster_hash, members_json, last_action, result_memory_id,
                    first_decided_at, last_decided_at, decision_count
             FROM cluster_decisions
             WHERE cluster_hash = ?",
        )
        .bind(&[cluster_hash.into()])?
        .first::<ClusterDecisionRow>(None)
        .await?;
    Ok(row.map(ClusterDecisionRow::into_decision))
}

/// Record a cluster decision in the cache. On first insert, seeds
/// `first_decided_at = last_decided_at = now()` and `decision_count = 1`.
/// On conflict (cluster already cached), increments `decision_count` and
/// refreshes `last_decided_at` / `last_action` / `result_memory_id`. The
/// last-write-wins on action keeps the cache honest if a future Haiku
/// version produces a different answer for the same member set (in which
/// case the new judgment supersedes the old — we trust the more recent
/// model run).
pub async fn upsert_cluster_decision(
    db: &D1Database,
    cluster_hash: &str,
    members_json: &str,
    last_action: &str,
    result_memory_id: Option<&str>,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    db.prepare(
        "INSERT INTO cluster_decisions
         (cluster_hash, members_json, last_action, result_memory_id,
          first_decided_at, last_decided_at, decision_count)
         VALUES (?, ?, ?, ?, ?, ?, 1)
         ON CONFLICT(cluster_hash) DO UPDATE SET
           last_action = excluded.last_action,
           result_memory_id = excluded.result_memory_id,
           last_decided_at = excluded.last_decided_at,
           decision_count = decision_count + 1",
    )
    .bind(&[
        cluster_hash.into(),
        members_json.into(),
        last_action.into(),
        match result_memory_id {
            Some(r) => r.into(),
            None => worker::wasm_bindgen::JsValue::NULL,
        },
        now.clone().into(),
        now.into(),
    ])?
    .run()
    .await?;
    Ok(())
}
