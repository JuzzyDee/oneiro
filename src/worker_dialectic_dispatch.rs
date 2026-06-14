// worker_dialectic_dispatch.rs — Dialectic Stage 3 (CLA-100).
//
// Reads `action` + `action_payload` from each Stage 2 audit row and runs
// the corresponding side effect:
//
//   keep    — no memory mutation; mark row dispatched
//   reframe — re-embed via Workers AI, upsert Vectorize, update D1
//             content/summary, preserve the original in memory_reframes
//   flag    — append a row to dialectic_flags (memory untouched)
//
// The validation gate lives in dialectic_validation (non-wasm-gated,
// native-testable). Everything else here is wasm-only because it
// touches worker::Env, D1, Vectorize, and Workers AI.
//
// Stage 2 stays purely judgmental; this module is the only place that
// mutates the memory store on the dialectic's behalf. memory_reframes
// is the safety net — every reframe is reversible via SQL because the
// original content + summary live in that table.
//
// Kill switch: `ONEIRO_DIALECTIC_DISPATCH` env var.
//   on      — real dispatch (default once burned in)
//   dry_run — validate + record dispatch_status="dry_run"; no mutation
//   off     — caller skips dispatch entirely
//
// Idempotency: dispatch only fires on decision rows where dispatched_at
// IS NULL (filtered by the caller). The dispatcher itself doesn't
// re-check — it trusts the caller's filter — but `mark_dispatched`
// always sets dispatched_at, so a row is never re-dispatched.

#![cfg(target_family = "wasm")]

use crate::dialectic_validation::validate_synthesis_payload;
use crate::memory::MemoryType;
use crate::{worker_embed, worker_store, worker_vectorize};
use serde_json::Value;
use uuid::Uuid;
use worker::{console_error, D1Database, Env, Result};

// ──── Dispatch mode (kill switch) ──────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchMode {
    /// Real dispatch. Default once burned-in.
    On,
    /// Validate the payload, mark `dispatched_at` + `dispatch_status="dry_run"`,
    /// do not touch the memory store. Useful for the first few nights of
    /// observing what Stage 3 *would* do before letting it act.
    DryRun,
    /// Skip dispatch entirely — Stage 2 audit rows accumulate unactioned.
    /// Effectively reverts the system to Stage 2 dark-launch.
    Off,
}

impl DispatchMode {
    /// Read `ONEIRO_DIALECTIC_DISPATCH` from the worker env.
    ///
    /// **Fail-closed:** missing or unrecognised values default to `DryRun`,
    /// not `On`. Stage 3 is the first dispatcher that mutates the memory
    /// store; a typo or forgotten secret should not silently enable
    /// destructive operations. Only an explicit `on` (or `live`) turns on
    /// real dispatch.
    pub fn from_env(env: &Env) -> Self {
        let raw = match env.var("ONEIRO_DIALECTIC_DISPATCH") {
            Ok(v) => v.to_string(),
            Err(_) => {
                worker::console_log!(
                    "ONEIRO_DIALECTIC_DISPATCH not set; defaulting to dry_run (fail-closed)"
                );
                return DispatchMode::DryRun;
            }
        };
        match raw.to_lowercase().as_str() {
            "on" | "live" => DispatchMode::On,
            "dry_run" | "dry-run" | "dryrun" => DispatchMode::DryRun,
            "off" | "disabled" => DispatchMode::Off,
            other => {
                worker::console_error!(
                    "ONEIRO_DIALECTIC_DISPATCH={:?} unrecognised; defaulting to dry_run",
                    other
                );
                DispatchMode::DryRun
            }
        }
    }
}

// ──── Outcome — what to record back to the audit row ───────────────────

#[derive(Debug, Clone, Copy)]
pub enum DispatchStatus {
    Success,
    DryRun,
    ValidationFailed,
    MemoryMissing,
    EmbedFailed,
    Error,
}

impl DispatchStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            DispatchStatus::Success => "success",
            DispatchStatus::DryRun => "dry_run",
            DispatchStatus::ValidationFailed => "validation_failed",
            DispatchStatus::MemoryMissing => "memory_missing",
            DispatchStatus::EmbedFailed => "embed_failed",
            DispatchStatus::Error => "error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DispatchOutcome {
    pub status: DispatchStatus,
    pub error: Option<String>,
}

impl DispatchOutcome {
    fn ok(status: DispatchStatus) -> Self {
        Self {
            status,
            error: None,
        }
    }

    fn err(status: DispatchStatus, msg: impl Into<String>) -> Self {
        Self {
            status,
            error: Some(msg.into()),
        }
    }
}

// ──── Entry point ──────────────────────────────────────────────────────

/// One end-to-end dispatch attempt for a single decision row. Returns
/// the outcome; the caller writes it back via `mark_dispatched`.
///
/// Validation runs in every mode — we want to know if a payload is
/// malformed even in dry-run, so prompt-iteration shows the bug before
/// dispatch goes live.
pub async fn dispatch_decision(
    env: &Env,
    db: &D1Database,
    decision_id: &str,
    memory_id: &str,
    action: &str,
    payload: &Value,
    mode: DispatchMode,
) -> Result<DispatchOutcome> {
    // 1. Validate first.
    if let Err(msg) = validate_synthesis_payload(action, payload) {
        return Ok(DispatchOutcome::err(DispatchStatus::ValidationFailed, msg));
    }

    // 2. Dry-run stops before any side effect.
    if mode == DispatchMode::DryRun {
        return Ok(DispatchOutcome::ok(DispatchStatus::DryRun));
    }

    // 3. Live dispatch by action.
    match action {
        "keep" => Ok(DispatchOutcome::ok(DispatchStatus::Success)),
        "reframe" => {
            let new_content = payload
                .get("new_content")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let new_summary = payload
                .get("new_summary")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            reframe_memory(env, db, decision_id, memory_id, new_content, new_summary).await
        }
        "flag" => {
            let note = payload.get("note").and_then(|v| v.as_str()).unwrap_or("");
            record_flag(db, decision_id, memory_id, note).await
        }
        // Already caught by validation, but the match must be exhaustive.
        other => Ok(DispatchOutcome::err(
            DispatchStatus::ValidationFailed,
            format!("unknown action reached dispatch: {}", other),
        )),
    }
}

// ──── Reframe — the only destructive path ──────────────────────────────

/// Apply a reframe end-to-end:
///
///   1. Fetch the original memory (preserve for audit + bail if missing)
///   2. Embed the new content (bail if Workers AI fails — no mutation yet)
///   3. Upsert Vectorize (bail if it fails — still no D1 mutation)
///   4. **Atomic D1 batch**: UPDATE `memories` content/summary + INSERT into
///      `memory_reframes`. D1 batches are transactional — either both
///      statements commit or neither does. This is the guarantee that
///      backs the "every reframe is reversible" safety claim: the
///      destructive update can never land without the audit row carrying
///      the original.
///
/// Order matters: embedding/Vectorize failures cost nothing in terms of
/// D1 state, so we run them first. If step 3 succeeds and step 4 fails,
/// the new vector is in place but D1 is untouched — a small inconsistency
/// window accepted per CLA-100 design, since the next dialectic pass will
/// re-embed the still-old content and self-heal.
async fn reframe_memory(
    env: &Env,
    db: &D1Database,
    decision_id: &str,
    memory_id: &str,
    new_content: &str,
    new_summary: &str,
) -> Result<DispatchOutcome> {
    // 1. Fetch original.
    let original = match worker_store::get(db, memory_id).await? {
        Some(m) => m,
        None => {
            return Ok(DispatchOutcome::err(
                DispatchStatus::MemoryMissing,
                format!("memory {} not found", memory_id),
            ));
        }
    };

    // Orientation axes are NOT embedded — nothing vector-searches them; recall loads
    // them straight from D1. The dialectic's V2 target is orientation, so skip embed
    // + Vectorize for an orientation reframe — running them would pollute the semantic
    // index with a vector that should not exist. Semantics still re-embed as before.
    if !matches!(original.memory_type, MemoryType::Orientation) {
        // 2. Embed new content.
        let new_embedding = match worker_embed::embed_document(env, new_content).await {
            Ok(e) => e,
            Err(e) => {
                return Ok(DispatchOutcome::err(
                    DispatchStatus::EmbedFailed,
                    format!("embed_document: {:?}", e),
                ));
            }
        };

        // 3. Upsert vectorize.
        if let Err(e) = worker_vectorize::upsert_one(env, memory_id, &new_embedding).await {
            return Ok(DispatchOutcome::err(
                DispatchStatus::EmbedFailed,
                format!("vectorize upsert: {:?}", e),
            ));
        }
    }

    // 4. Atomic D1 batch — UPDATE memories + INSERT memory_reframes.
    //    Either both land or neither does. Without atomicity, an audit
    //    insert that fails after the UPDATE succeeded would destroy the
    //    only copy of the original content. We do not accept that path.
    let now = chrono::Utc::now().to_rfc3339();
    let reframe_id = Uuid::new_v4().to_string();

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
            memory_id.into(),
        ])?;

    let audit_stmt = db
        .prepare(
            "INSERT INTO memory_reframes
                (reframe_id, memory_id, decision_id,
                 old_content, old_summary, new_content, new_summary, reframed_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&[
            reframe_id.into(),
            memory_id.into(),
            decision_id.into(),
            original.content.into(),
            original.summary.into(),
            new_content.into(),
            new_summary.into(),
            now.into(),
        ])?;

    if let Err(e) = db.batch(vec![update_stmt, audit_stmt]).await {
        // D1 batch is atomic — if it failed, neither statement committed.
        // The memory is still in its pre-reframe state. The vector half
        // is the only thing that already landed; next dialectic pass
        // will surface and re-process.
        console_error!(
            "reframe batch failed (memory {} unchanged, vector stale): {:?}",
            memory_id,
            e
        );
        return Ok(DispatchOutcome::err(
            DispatchStatus::Error,
            format!("reframe batch failed: {:?}", e),
        ));
    }

    Ok(DispatchOutcome::ok(DispatchStatus::Success))
}

// ──── Flag — append to dialectic_flags ─────────────────────────────────

async fn record_flag(
    db: &D1Database,
    decision_id: &str,
    memory_id: &str,
    note: &str,
) -> Result<DispatchOutcome> {
    let flag_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    if let Err(e) = db
        .prepare(
            "INSERT INTO dialectic_flags
                (flag_id, decision_id, memory_id, note, flagged_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&[
            flag_id.into(),
            decision_id.into(),
            memory_id.into(),
            note.into(),
            now.into(),
        ])?
        .run()
        .await
    {
        return Ok(DispatchOutcome::err(
            DispatchStatus::Error,
            format!("dialectic_flags insert: {:?}", e),
        ));
    }

    Ok(DispatchOutcome::ok(DispatchStatus::Success))
}

// ──── Audit row update ─────────────────────────────────────────────────

/// Mark a decision row as dispatched. Always sets `dispatched_at`;
/// `dispatch_status` records the outcome; `dispatch_error` carries the
/// error message when status is not success/dry_run.
///
/// The `WHERE dispatched_at IS NULL` guard makes this primitive idempotent
/// on its own — re-calling it on an already-dispatched row is a safe
/// no-op rather than overwriting an earlier outcome. Future replay/
/// catch-up paths can rely on this without holding their own filter.
pub async fn mark_dispatched(
    db: &D1Database,
    decision_id: &str,
    outcome: &DispatchOutcome,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    let err_param = match &outcome.error {
        Some(e) => e.as_str().into(),
        None => worker::wasm_bindgen::JsValue::NULL,
    };
    db.prepare(
        "UPDATE dialectic_decisions
            SET dispatched_at = ?, dispatch_status = ?, dispatch_error = ?
         WHERE decision_id = ? AND dispatched_at IS NULL",
    )
    .bind(&[
        now.into(),
        outcome.status.as_str().into(),
        err_param,
        decision_id.into(),
    ])?
    .run()
    .await?;
    Ok(())
}
