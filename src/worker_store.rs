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
         WHERE id IN ({})",
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

/// All orientation memories. Always loaded — the core of identity.
pub async fn get_orientation(db: &D1Database) -> Result<Vec<Memory>> {
    let rows: Vec<MemoryRow> = db
        .prepare(
            "SELECT id, memory_type, content, summary, created_at, last_accessed,
                    access_count, strength, stability, entity, tags, image_hash,
                    image_mime, recorded_by
             FROM memories
             WHERE memory_type = 'orientation'
             ORDER BY created_at ASC",
        )
        .bind(&[])?
        .all()
        .await?
        .results()?;
    Ok(rows.into_iter().map(MemoryRow::into_memory).collect())
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

/// Update a memory's content + summary (after a re-embedding decision).
/// Does NOT touch the embedding — Vectorize upsert is the caller's job
/// since we don't generate embeddings inside the store.
pub async fn reframe(
    db: &D1Database,
    id: &str,
    new_content: &str,
    new_summary: &str,
) -> Result<bool> {
    let now = chrono::Utc::now().to_rfc3339();
    let result = db
        .prepare(
            "UPDATE memories
             SET content = ?, summary = ?, last_accessed = ?
             WHERE id = ?",
        )
        .bind(&[new_content.into(), new_summary.into(), now.into(), id.into()])?
        .run()
        .await?;
    Ok(result.meta().ok().flatten().and_then(|m| m.changes).unwrap_or(0) > 0)
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
