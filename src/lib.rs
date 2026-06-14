// lib.rs — Oneiro as a Cloudflare Worker (CLA-84).
//
// Public surface, by design, is just POST /mcp (authenticated MCP via
// JSON-RPC). Everything else falls through to a placeholder string —
// no anonymous read/write paths exist. The /test/* endpoints used
// during the migration's earlier phases have been removed; their
// behaviour is now reachable only through authenticated tools/call
// invocations.

#![cfg(target_family = "wasm")]

// Universal types — shared with the native bins via the same source files.
mod api_key;
mod audit;
mod dialectic_validation;
mod embed;
mod hybrid;
mod key_rate;
mod memory;

// Worker-side modules (wasm32-only).
mod worker_audit;
mod worker_auth_ctx;
mod worker_dialectic;
mod worker_dialectic_audit;
mod worker_dialectic_dispatch;
mod worker_embed;
mod worker_mcp;
mod worker_mmr;
mod worker_oauth;
mod worker_orient;
mod worker_orient_distill;
mod worker_rem;
mod worker_encode;
mod worker_encode_batch;
mod worker_cscc;
mod worker_adas;
mod worker_rem_audit;
mod worker_store;
mod worker_vectorize;
mod worker_version;

use worker::{
    event, Context, D1Database, Env, MessageBatch, MessageBuilder, Method, Request, Response,
    Result, ScheduleContext, ScheduledEvent,
};

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let url = req.url()?;
    let path = url.path().to_string();
    let method = req.method();

    // base_url for the OAuth metadata responses — must match how clients
    // arrive at the worker. Pulled from the Host header so it works
    // identically with custom domains and the workers.dev subdomain.
    let host = req
        .headers()
        .get("host")?
        .unwrap_or_else(|| "oneiro.juzzydee.workers.dev".to_string());
    let base_url = format!("https://{}", host);

    match (method, path.as_str()) {
        // MCP — authenticated, scope-gated, audited. Accepts both
        // OAuth bearer tokens and service API keys.
        (Method::Post, "/mcp") => mcp_endpoint(&env, &mut req).await,

        // Orientation — plain-text orientation payload for the
        // SessionStart/PreCompact hook (CLA-105) to inject before any
        // tool evaluation fires. Authed via service API key OR OAuth
        // bearer — same validator as /mcp. Read-only, no scope check
        // beyond "valid bearer" — orientation memories are pinned and
        // identity-bearing, suitable for any authenticated caller.
        (Method::Get, "/orientation") => orientation_endpoint(&env, &req).await,

        // Encode — authenticated write surface for the PostCompact hook
        // (CLA-116). Captures a raw episodic with NO embedding; the nightly
        // Stage-1 pass distils + embeds the clean output, so we never embed
        // the muddy multi-topic summary. See docs/orientation-redesign.md §5/§10.
        (Method::Post, "/encode") => encode_endpoint(&env, &mut req).await,

        // Admin: run Stage-1 encode over a chunk of the unintegrated backlog
        // (CLA-117 runner / CLA-122 migration). Returns the per-episodic
        // decisions so a run can be inspected. Gated on `encode` — a migration
        // tool, not the production trigger (that's a cron/queue).
        (Method::Post, "/admin/encode-run") => admin_encode_endpoint(&env, &mut req).await,

        // Admin: read recent encode-call autopsies (CLA-132). The forensic capture
        // of what the judge actually returned — for diagnosing the first-submission
        // whiff. Optional `?limit=N` (default 20). Gated on `encode`.
        (Method::Get, "/admin/encode-diagnostics") => admin_encode_diagnostics_endpoint(&env, &req).await,

        // Admin: read-only proximity clustering of the live semantic layer
        // (CLA-134). NO Haiku, NO writes — calibration only. Pulls every
        // non-superseded semantic's vector, computes exact pairwise cosine,
        // union-finds at `?threshold=` (similarity, default 0.95) into clusters
        // of `?min_size=` (default 2). Lets us tune the consolidation threshold
        // against the real store before building the write-bearing pass.
        (Method::Get, "/admin/cluster-preview") => admin_cluster_preview_endpoint(&env, &req).await,

        // Admin: cscc-calibrate (CLA-134). DRY-RUN merge calibrator — clusters the
        // semantic layer at `?threshold=` (default 0.90) and runs each cluster
        // through the Haiku merge/keep-distinct judge. NO writes; returns verdicts
        // + reasons so the judgment can be tuned before execution plumbing exists.
        (Method::Get, "/admin/cscc-calibrate") => admin_cscc_calibrate_endpoint(&env, &req).await,

        // Admin: cscc-execute (CLA-134). Clusters + judges like calibrate, then
        // folds each confirmed MERGE into its keeper. `?commit=false` (default) is
        // a DRY RUN — no writes, shows the fold plan; `commit=true` writes.
        (Method::Get, "/admin/cscc-execute") => admin_cscc_execute_endpoint(&env, &req).await,

        // Admin: split-preview (CLA-134, ADAS). READ-ONLY conflation detector —
        // ranks live semantics by over-creation (orientation axes fed) + length,
        // no Haiku, no writes. Calibrate-first for the split half of defrag.
        (Method::Get, "/admin/split-preview") => admin_split_preview_endpoint(&env, &req).await,

        // Admin: split-judge (CLA-134, ADAS). DRY-RUN — Haiku reads each
        // pre-filtered candidate whole and judges keep-vs-split. No writes;
        // surfaces the crown-jewel `protected` flag beside each verdict.
        (Method::Get, "/admin/split-judge") => admin_split_judge_endpoint(&env, &req).await,

        // Admin: decompose-preview (CLA-134 / encode rebuild, Step 0). Runs the
        // decompose stage on one episodic (?episodic_id=<id or 8-char prefix>)
        // and returns its atomic knowledge-units — NO judge, NO writes. Lets us
        // eyeball atomisation granularity on a known failure before building the
        // per-unit judge. Gated on `encode`.
        (Method::Get, "/admin/decompose-preview") => admin_decompose_preview_endpoint(&env, &req).await,

        // Admin: decomposed encode (CLA-134 / encode rebuild, Step 1).
        // ?episodic_id=<id|prefix> decompose → per-unit judge → filings.
        // DRY RUN by default; ?commit=true actually dispatches (writes semantics).
        // The fix for the monolith whiff: one small judge call per unit, no
        // truncation possible. Gated on `encode`.
        (Method::Post, "/admin/encode-decomposed") => admin_encode_decomposed_endpoint(&env, &req).await,

        // Admin: run Stage-2 distillation (semantic → orientation) over the
        // semantics edged since a cutoff (CLA-125). Body: { since?, limit? }.
        // Returns the per-semantic orientation decisions so a run can be
        // inspected before any cron. Gated on `encode` — admin cognitive-write
        // tooling, run manually with the orient/hook key during calibration.
        (Method::Post, "/admin/orient-run") => admin_orient_endpoint(&env, &mut req).await,

        // Admin: run Stage-C — the Whittling (CLA-126). Pure-query re-settle of
        // the always-loaded set to ~20 by Recency × Frequency × Meaning; returns
        // the full ranking (kept + per-factor) so the cut is auditable. No body.
        (Method::Post, "/admin/orient-whittle") => admin_orient_whittle_endpoint(&env, &mut req).await,

        // Admin: one-off backfill (CLA-126). Rates M on pre-existing axes and
        // carves any over the per-axis budget, then whittles. Run once after the
        // 0013 migration; idempotent on re-run. No body.
        (Method::Post, "/admin/orient-backfill") => admin_orient_backfill_endpoint(&env, &mut req).await,

        // Admin: apply the cold structural pass (CLA-126). Re-scopes core to the
        // genuine lenses, reframes them with the synthesised content, carves the
        // over-budget references. Body is the proposal JSON {axes:[...]}.
        (Method::Post, "/admin/orient-apply-pillars") => admin_orient_apply_pillars_endpoint(&env, &mut req).await,

        // OAuth 2.1 — Authorization Code + Client Credentials grants.
        (Method::Get, "/.well-known/oauth-protected-resource") => {
            worker_oauth::protected_resource_metadata(&base_url)
        }
        (Method::Get, "/.well-known/oauth-authorization-server") => {
            worker_oauth::authorization_server_metadata(&base_url)
        }
        (Method::Get, "/authorize") => render_consent_page(&env, &req).await,
        (Method::Post, "/authorize") => handle_authorize_form(&env, &mut req).await,
        (Method::Post, "/token") => handle_token_form(&env, &mut req).await,

        // Liveness.
        (Method::Get, "/healthz") => Response::ok("ok"),

        // Default — no info leakage.
        _ => Response::ok("oneiro"),
    }
}

/// POST /mcp — the MCP server endpoint. Validates Bearer (OAuth OR
/// service API key), sets AUTH_CTX scope, hands off to worker_mcp::handle
/// for JSON-RPC dispatch.
async fn mcp_endpoint(env: &Env, req: &mut Request) -> Result<Response> {
    let bearer = req
        .headers()
        .get("authorization")?
        .and_then(|h| h.strip_prefix("Bearer ").map(|s| s.to_string()));

    let Some(bearer) = bearer else {
        return Response::error("Missing Authorization: Bearer <key>", 401);
    };
    let Some(auth) = worker_auth_ctx::validate_bearer(env, &bearer).await else {
        return Response::error("Invalid or unknown bearer token", 401);
    };

    let body = req.text().await?;
    worker_mcp::handle(env, &body, auth).await
}

/// GET /orientation — plain-text orientation payload for the
/// SessionStart/PreCompact hook (CLA-105). Returns the same orientation
/// block that `recall_orient` would surface, formatted for direct
/// injection as system context. No recent episodics — orientation only,
/// because the hook fires automatically (potentially many times per
/// session) and surfacing episodics there would inflate access counts
/// and pollute the Hebbian co-activation signal with non-conscious
/// surfacing noise. The model can call `recall_orient` deliberately if
/// it wants the recent half too.
async fn orientation_endpoint(env: &Env, req: &Request) -> Result<Response> {
    let bearer = req
        .headers()
        .get("authorization")?
        .and_then(|h| h.strip_prefix("Bearer ").map(|s| s.to_string()));

    let Some(bearer) = bearer else {
        return Response::error("Missing Authorization: Bearer <key>", 401);
    };
    let Some(auth) = worker_auth_ctx::validate_bearer(env, &bearer).await else {
        return Response::error("Invalid or unknown bearer token", 401);
    };

    let db = env.d1("DB")?;

    // Scope-gate on recall_orient — the same capability the model's
    // recall_orient tool requires. Makes orientation access explicit in the
    // role allowlist (CLA-116) rather than implicitly open to any bearer.
    let result: std::result::Result<String, (u16, String)> = worker_auth_ctx::AUTH_CTX
        .scope(auth, async {
            worker_auth_ctx::check_scope(&db, "recall_orient")
                .await
                .map_err(|e| (403u16, e))?;
            let orientation = worker_store::get_active_orientation(&db)
                .await
                .map_err(|e| (500u16, format!("get_active_orientation: {:?}", e)))?;
            let counts = worker_store::count_by_type(&db).await.unwrap_or((0, 0, 0));
            Ok(worker_orient::format_payload(&orientation, None, counts))
        })
        .await;

    match result {
        Ok(payload) => {
            let mut resp = Response::ok(payload)?;
            resp.headers_mut()
                .set("content-type", "text/plain; charset=utf-8")?;
            Ok(resp)
        }
        Err((code, msg)) => Response::error(msg, code),
    }
}

/// Body for POST /encode. `content` is the raw captured text (a compaction
/// summary from the PostCompact hook). `summary` is optional — if the caller
/// can't distil one we derive a placeholder; the nightly Stage-1 pass
/// produces the real distillation. `tags`/`entity` are optional hints.
#[derive(serde::Deserialize)]
struct EncodeArgs {
    content: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    entity: Option<String>,
}

/// Placeholder summary when the caller doesn't supply one: first line,
/// trimmed, capped. Real distillation happens nightly (CLA-117).
fn derive_summary(content: &str) -> String {
    let first = content.lines().next().unwrap_or(content).trim();
    let s: String = first.chars().take(80).collect();
    if s.is_empty() {
        "(captured summary)".to_string()
    } else {
        s
    }
}

/// POST /encode — authenticated episodic capture for the PostCompact hook
/// (CLA-116). Writes the raw episodic to D1 with NO embedding: the nightly
/// Stage-1 pass (CLA-117) distils it into a semantic and embeds the clean
/// output, so the muddy multi-topic summary is never embedded. `integrated`
/// defaults to 0, marking the row for that pass. Write-gated by reusing the
/// `remember` scope so a read-only key cannot write here.
async fn encode_endpoint(env: &Env, req: &mut Request) -> Result<Response> {
    let bearer = req
        .headers()
        .get("authorization")?
        .and_then(|h| h.strip_prefix("Bearer ").map(|s| s.to_string()));
    let Some(bearer) = bearer else {
        return Response::error("Missing Authorization: Bearer <key>", 401);
    };
    let Some(auth) = worker_auth_ctx::validate_bearer(env, &bearer).await else {
        return Response::error("Invalid or unknown bearer token", 401);
    };

    let body = req.text().await?;
    let payload: EncodeArgs = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => return Response::error(format!("invalid encode body: {}", e), 400),
    };
    if payload.content.trim().is_empty() {
        return Response::error("encode: `content` is required", 400);
    }

    let db = env.d1("DB")?;

    // Resolve summary + recorded_by inside AUTH_CTX (scope-gated + audited),
    // then ENQUEUE — the queue consumer does the write + encode. The hook (and
    // the reflect tool, which shares this path) gets a fast response; the slow
    // Haiku encode runs in the consumer, durably and with retries.
    let result: std::result::Result<worker_encode::CaptureMessage, (u16, String)> =
        worker_auth_ctx::AUTH_CTX
            .scope(auth, async move {
                worker_auth_ctx::check_scope(&db, "encode")
                    .await
                    .map_err(|e| (403u16, e))?;
                let summary = payload
                    .summary
                    .unwrap_or_else(|| derive_summary(&payload.content));
                Ok(worker_encode::CaptureMessage {
                    content: payload.content,
                    summary,
                    entity: payload.entity,
                    tags: payload.tags,
                    recorded_by: worker_auth_ctx::current_recorded_by(),
                })
            })
            .await;

    let msg = match result {
        Ok(m) => m,
        Err((code, text)) => return Response::error(text, code),
    };

    env.queue("CAPTURE_QUEUE")?
        .send(worker_encode::QueueMessage::Capture(msg))
        .await?;

    let mut resp = Response::ok("{\"ok\":true,\"queued\":true}")?;
    resp.headers_mut().set("content-type", "application/json")?;
    Ok(resp)
}

#[derive(serde::Deserialize)]
struct EncodeRunArgs {
    #[serde(default = "default_encode_limit")]
    limit: u32,
    #[serde(default)]
    mark_first: bool,
    /// Target one episodic by id — encode just that one and let the probe fire
    /// on it, instead of draining the backlog. Lets us measure a specific case
    /// (the compaction episodic) with no other queued episodics in the way,
    /// regardless of what's been remembered since (CLA-131).
    #[serde(default)]
    episodic_id: Option<String>,
}
fn default_encode_limit() -> u32 {
    10
}

#[derive(serde::Deserialize)]
struct OrientRunArgs {
    /// RFC3339 cutoff; semantics edged since this are distilled. Defaults to
    /// 24h ago (the steady-state nightly window). Pass an explicit value to
    /// scope a calibration run (e.g. "this morning" to skip the rebuild's edges).
    #[serde(default)]
    since: Option<String>,
    #[serde(default = "default_orient_limit")]
    limit: u32,
}
fn default_orient_limit() -> u32 {
    20
}

/// POST /admin/encode-run — run Stage-1 encode over a chunk of the
/// unintegrated backlog (CLA-117 runner / CLA-122 migration). Optional body:
/// `{ "limit": 5, "mark_first": true }`. `mark_first` runs the
/// already-consolidated pre-step once. Returns the per-episodic decisions.
/// Gated on `encode` so the hook key can drive the one-time migration; this
/// is a migration tool, not the production trigger — lock or remove it once
/// the backfill is done.
async fn admin_encode_endpoint(env: &Env, req: &mut Request) -> Result<Response> {
    let bearer = req
        .headers()
        .get("authorization")?
        .and_then(|h| h.strip_prefix("Bearer ").map(|s| s.to_string()));
    let Some(bearer) = bearer else {
        return Response::error("Missing Authorization: Bearer <key>", 401);
    };
    let Some(auth) = worker_auth_ctx::validate_bearer(env, &bearer).await else {
        return Response::error("Invalid or unknown bearer token", 401);
    };

    let body = req.text().await?;
    let args: EncodeRunArgs = if body.trim().is_empty() {
        EncodeRunArgs { limit: default_encode_limit(), mark_first: false, episodic_id: None }
    } else {
        match serde_json::from_str(&body) {
            Ok(a) => a,
            Err(e) => return Response::error(format!("invalid encode-run body: {}", e), 400),
        }
    };

    let db = env.d1("DB")?;
    let result: std::result::Result<serde_json::Value, (u16, String)> = worker_auth_ctx::AUTH_CTX
        .scope(auth, async move {
            worker_auth_ctx::check_scope(&db, "encode")
                .await
                .map_err(|e| (403u16, e))?;

            let marked = if args.mark_first {
                worker_store::mark_consolidated_episodics_integrated(&db)
                    .await
                    .map_err(|e| (500u16, format!("mark step: {:?}", e)))?
            } else {
                0
            };

            let outcomes = if let Some(ref eid) = args.episodic_id {
                // Targeted probe (CLA-131): encode exactly this episodic and let
                // the probe fire on it, ignoring whatever else sits in the backlog.
                let ep = worker_store::get(&db, eid)
                    .await
                    .map_err(|e| (500u16, format!("get episodic: {:?}", e)))?
                    .ok_or((404u16, format!("episodic {} not found", eid)))?;
                vec![
                    worker_encode::encode_one(env, &db, &ep)
                        .await
                        .map_err(|e| (500u16, format!("encode_one: {:?}", e)))?,
                ]
            } else {
                worker_encode::run_encode_batch(env, &db, args.limit)
                    .await
                    .map_err(|e| (500u16, format!("encode batch: {:?}", e)))?
            };

            let decisions: Vec<serde_json::Value> = outcomes
                .iter()
                .map(|o| {
                    serde_json::json!({
                        "episodic": &o.episodic_id[..o.episodic_id.len().min(8)],
                        "decisions": o
                            .decisions
                            .iter()
                            .map(|d| serde_json::json!({
                                "action": d.action,
                                "semantic": d.semantic_id.as_deref().map(|s| &s[..s.len().min(8)]),
                                "flavour": d.flavour,
                            }))
                            .collect::<Vec<_>>(),
                    })
                })
                .collect();

            Ok(serde_json::json!({
                "marked_already_consolidated": marked,
                "processed": decisions.len(),
                "decisions": decisions,
            }))
        })
        .await;

    match result {
        Ok(v) => {
            let mut resp = Response::ok(v.to_string())?;
            resp.headers_mut().set("content-type", "application/json")?;
            Ok(resp)
        }
        Err((code, msg)) => Response::error(msg, code),
    }
}

/// POST /admin/orient-run — run Stage-2 distillation (semantic → orientation)
/// over the semantics edged since a cutoff (CLA-125). Optional body:
/// `{ "since": "2026-06-09T00:00:00Z", "limit": 20 }`. `since` defaults to 24h
/// ago. Returns the per-semantic orientation decisions for inspection. Gated on
/// `encode` — calibration/admin tooling, not the production cron trigger.
async fn admin_orient_endpoint(env: &Env, req: &mut Request) -> Result<Response> {
    let bearer = req
        .headers()
        .get("authorization")?
        .and_then(|h| h.strip_prefix("Bearer ").map(|s| s.to_string()));
    let Some(bearer) = bearer else {
        return Response::error("Missing Authorization: Bearer <key>", 401);
    };
    let Some(auth) = worker_auth_ctx::validate_bearer(env, &bearer).await else {
        return Response::error("Invalid or unknown bearer token", 401);
    };

    let body = req.text().await?;
    let args: OrientRunArgs = if body.trim().is_empty() {
        OrientRunArgs { since: None, limit: default_orient_limit() }
    } else {
        match serde_json::from_str(&body) {
            Ok(a) => a,
            Err(e) => return Response::error(format!("invalid orient-run body: {}", e), 400),
        }
    };
    let since = args
        .since
        .unwrap_or_else(|| (chrono::Utc::now() - chrono::Duration::hours(24)).to_rfc3339());

    let db = env.d1("DB")?;
    let result: std::result::Result<serde_json::Value, (u16, String)> = worker_auth_ctx::AUTH_CTX
        .scope(auth, async move {
            worker_auth_ctx::check_scope(&db, "encode")
                .await
                .map_err(|e| (403u16, e))?;

            let outcomes = worker_orient_distill::run_orient_batch(env, &db, &since, args.limit)
                .await
                .map_err(|e| (500u16, format!("orient batch: {:?}", e)))?;

            let decisions: Vec<serde_json::Value> = outcomes
                .iter()
                .map(|o| {
                    serde_json::json!({
                        "semantic": &o.semantic_id[..o.semantic_id.len().min(8)],
                        "decisions": o
                            .decisions
                            .iter()
                            .map(|d| serde_json::json!({
                                "action": d.action,
                                "orientation": d.orientation_id.as_deref().map(|s| &s[..s.len().min(8)]),
                                "flavour": d.flavour,
                                "rationale": d.rationale,
                            }))
                            .collect::<Vec<_>>(),
                    })
                })
                .collect();

            Ok(serde_json::json!({
                "since": since,
                "processed": decisions.len(),
                "decisions": decisions,
            }))
        })
        .await;

    match result {
        Ok(v) => {
            let mut resp = Response::ok(v.to_string())?;
            resp.headers_mut().set("content-type", "application/json")?;
            Ok(resp)
        }
        Err((code, msg)) => Response::error(msg, code),
    }
}

/// Format a Whittle ranking as audit JSON — counts plus a best-first row per
/// axis with its kept flag and the R/F/M factors that decided the cut. Shared by
/// the whittle and backfill endpoints. Scores/days rounded for readability.
fn whittle_json(ranking: &[worker_orient_distill::WhittleRanking]) -> serde_json::Value {
    let active = ranking.iter().filter(|r| r.kept).count();
    serde_json::json!({
        "total": ranking.len(),
        "active": active,
        "dormant": ranking.len() - active,
        "ranking": ranking
            .iter()
            .map(|r| serde_json::json!({
                "id": &r.id[..r.id.len().min(8)],
                "summary": r.summary,
                "kept": r.kept,
                "core": r.core,
                "score": (r.score * 100.0).round() / 100.0,
                "days_since": (r.days_since * 10.0).round() / 10.0,
                "freq": r.freq,
                "meaning": r.meaning,
                "rubric": [r.relational, r.durability, r.irreplaceable, r.cost],
            }))
            .collect::<Vec<_>>(),
    })
}

/// POST /admin/orient-whittle — Stage C, the Whittling (CLA-126). Re-settles the
/// always-loaded set to ~20 and returns the full ranking for audit. Gated on
/// `encode`, like the other admin cognitive-write tooling.
async fn admin_orient_whittle_endpoint(env: &Env, req: &mut Request) -> Result<Response> {
    let bearer = req
        .headers()
        .get("authorization")?
        .and_then(|h| h.strip_prefix("Bearer ").map(|s| s.to_string()));
    let Some(bearer) = bearer else {
        return Response::error("Missing Authorization: Bearer <key>", 401);
    };
    let Some(auth) = worker_auth_ctx::validate_bearer(env, &bearer).await else {
        return Response::error("Invalid or unknown bearer token", 401);
    };

    let db = env.d1("DB")?;
    let result: std::result::Result<serde_json::Value, (u16, String)> = worker_auth_ctx::AUTH_CTX
        .scope(auth, async move {
            worker_auth_ctx::check_scope(&db, "encode")
                .await
                .map_err(|e| (403u16, e))?;
            let ranking = worker_orient_distill::run_whittle(&db)
                .await
                .map_err(|e| (500u16, format!("whittle: {:?}", e)))?;
            Ok(whittle_json(&ranking))
        })
        .await;

    match result {
        Ok(v) => {
            let mut resp = Response::ok(v.to_string())?;
            resp.headers_mut().set("content-type", "application/json")?;
            Ok(resp)
        }
        Err((code, msg)) => Response::error(msg, code),
    }
}

/// POST /admin/orient-backfill — one-off (CLA-126). Rates M on pre-existing axes
/// and carves any over the per-axis budget, then whittles. Returns the per-axis
/// backfill actions and the resulting ranking. Gated on `encode`; idempotent.
async fn admin_orient_backfill_endpoint(env: &Env, req: &mut Request) -> Result<Response> {
    let bearer = req
        .headers()
        .get("authorization")?
        .and_then(|h| h.strip_prefix("Bearer ").map(|s| s.to_string()));
    let Some(bearer) = bearer else {
        return Response::error("Missing Authorization: Bearer <key>", 401);
    };
    let Some(auth) = worker_auth_ctx::validate_bearer(env, &bearer).await else {
        return Response::error("Invalid or unknown bearer token", 401);
    };

    let db = env.d1("DB")?;
    let result: std::result::Result<serde_json::Value, (u16, String)> = worker_auth_ctx::AUTH_CTX
        .scope(auth, async move {
            worker_auth_ctx::check_scope(&db, "encode")
                .await
                .map_err(|e| (403u16, e))?;
            let outcomes = worker_orient_distill::run_orient_backfill(env, &db)
                .await
                .map_err(|e| (500u16, format!("orient backfill: {:?}", e)))?;
            let ranking = worker_orient_distill::run_whittle(&db)
                .await
                .map_err(|e| (500u16, format!("whittle: {:?}", e)))?;

            let axes: Vec<serde_json::Value> = outcomes
                .iter()
                .map(|o| serde_json::json!({
                    "id": &o.id[..o.id.len().min(8)],
                    "summary": o.summary,
                    "meaning_set": o.meaning_set,
                    "rubric_set": o.rubric_set,
                    "rubric": [o.relational, o.durability, o.irreplaceable, o.cost],
                    "carved": o.carved.map(|(f, t)| serde_json::json!({ "from": f, "to": t })),
                }))
                .collect();

            Ok(serde_json::json!({
                "backfilled": axes.len(),
                "axes": axes,
                "whittle": whittle_json(&ranking),
            }))
        })
        .await;

    match result {
        Ok(v) => {
            let mut resp = Response::ok(v.to_string())?;
            resp.headers_mut().set("content-type", "application/json")?;
            Ok(resp)
        }
        Err((code, msg)) => Response::error(msg, code),
    }
}

/// POST /admin/orient-apply-pillars — apply the cold structural pass (CLA-126).
/// Body is the proposal JSON `{ "axes": [ { id, classification, synthesized_content?, rationale? } ] }`.
/// Re-scopes core, reframes the lenses (audited/reversible), carves over-budget
/// references, then whittles. Gated on `encode`. Returns per-axis actions + ranking.
async fn admin_orient_apply_pillars_endpoint(env: &Env, req: &mut Request) -> Result<Response> {
    let bearer = req
        .headers()
        .get("authorization")?
        .and_then(|h| h.strip_prefix("Bearer ").map(|s| s.to_string()));
    let Some(bearer) = bearer else {
        return Response::error("Missing Authorization: Bearer <key>", 401);
    };
    let Some(auth) = worker_auth_ctx::validate_bearer(env, &bearer).await else {
        return Response::error("Invalid or unknown bearer token", 401);
    };

    let body = req.text().await?;
    let parsed: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return Response::error(format!("invalid apply-pillars body: {}", e), 400),
    };
    let decisions: Vec<worker_orient_distill::PillarDecision> =
        match serde_json::from_value(parsed.get("axes").cloned().unwrap_or(serde_json::Value::Null)) {
            Ok(d) => d,
            Err(e) => return Response::error(format!("invalid axes: {}", e), 400),
        };

    let db = env.d1("DB")?;
    let result: std::result::Result<serde_json::Value, (u16, String)> = worker_auth_ctx::AUTH_CTX
        .scope(auth, async move {
            worker_auth_ctx::check_scope(&db, "encode")
                .await
                .map_err(|e| (403u16, e))?;
            let outcomes = worker_orient_distill::apply_pillars(env, &db, &decisions)
                .await
                .map_err(|e| (500u16, format!("apply pillars: {:?}", e)))?;
            let ranking = worker_orient_distill::run_whittle(&db)
                .await
                .map_err(|e| (500u16, format!("whittle: {:?}", e)))?;
            let applied: Vec<serde_json::Value> = outcomes
                .iter()
                .map(|o| serde_json::json!({
                    "id": &o.id[..o.id.len().min(8)],
                    "core": o.core,
                    "action": o.action,
                    "chars": o.chars,
                }))
                .collect();
            Ok(serde_json::json!({
                "applied": applied.len(),
                "axes": applied,
                "whittle": whittle_json(&ranking),
            }))
        })
        .await;

    match result {
        Ok(v) => {
            let mut resp = Response::ok(v.to_string())?;
            resp.headers_mut().set("content-type", "application/json")?;
            Ok(resp)
        }
        Err((code, msg)) => Response::error(msg, code),
    }
}

/// GET /admin/encode-diagnostics?limit=N — recent encode-call autopsies (CLA-132).
/// The forensic record of what each judge call returned (stop_reason, usage,
/// tool_use presence, content blocks, decision count) for both the batch and sync
/// paths — so a failing first-submission and a succeeding re-run can be compared.
async fn admin_encode_diagnostics_endpoint(env: &Env, req: &Request) -> Result<Response> {
    let bearer = req
        .headers()
        .get("authorization")?
        .and_then(|h| h.strip_prefix("Bearer ").map(|s| s.to_string()));
    let Some(bearer) = bearer else {
        return Response::error("Missing Authorization: Bearer <key>", 401);
    };
    let Some(auth) = worker_auth_ctx::validate_bearer(env, &bearer).await else {
        return Response::error("Invalid or unknown bearer token", 401);
    };
    let limit: u32 = req
        .url()
        .ok()
        .and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == "limit")
                .map(|(_, v)| v.into_owned())
        })
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    let db = env.d1("DB")?;
    let result: std::result::Result<serde_json::Value, (u16, String)> = worker_auth_ctx::AUTH_CTX
        .scope(auth, async move {
            worker_auth_ctx::check_scope(&db, "encode")
                .await
                .map_err(|e| (403u16, e))?;
            let rows = worker_store::recent_encode_diagnostics(&db, limit)
                .await
                .map_err(|e| (500u16, format!("diagnostics: {:?}", e)))?;
            let out: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| serde_json::json!({
                    "episodic": &r.episodic_id[..r.episodic_id.len().min(8)],
                    "source": r.source,
                    "candidates": r.candidate_count,
                    "chars": r.content_chars,
                    "http": r.http_status,
                    "stop_reason": r.stop_reason,
                    "in": r.usage_in,
                    "out": r.usage_out,
                    "tool_use": r.has_tool_use.map(|v| v == 1),
                    "blocks": r.content_types,
                    "decisions": r.decision_count,
                    "at": r.created_at,
                }))
                .collect();
            Ok(serde_json::json!({ "count": out.len(), "diagnostics": out }))
        })
        .await;

    match result {
        Ok(v) => {
            let mut resp = Response::ok(v.to_string())?;
            resp.headers_mut().set("content-type", "application/json")?;
            Ok(resp)
        }
        Err((code, msg)) => Response::error(msg, code),
    }
}

/// GET /admin/split-judge?min_axes=2&min_len=800&limit=20 — ADAS split-judge
/// DRY-RUN (CLA-134). Pre-filters candidates (over-creation OR length), then Haiku
/// reads each WHOLE and judges keep-vs-split. NO writes. Surfaces `protected`
/// (feeds a core/pinned axis) beside each verdict so the crown-jewel guard is
/// visible. Calibrate the prompt on this output before separation is built.
/// Gated on `encode`.
async fn admin_split_judge_endpoint(env: &Env, req: &Request) -> Result<Response> {
    let bearer = req
        .headers()
        .get("authorization")?
        .and_then(|h| h.strip_prefix("Bearer ").map(|s| s.to_string()));
    let Some(bearer) = bearer else {
        return Response::error("Missing Authorization: Bearer <key>", 401);
    };
    let Some(auth) = worker_auth_ctx::validate_bearer(env, &bearer).await else {
        return Response::error("Invalid or unknown bearer token", 401);
    };

    let url = req.url().ok();
    let param = |key: &str| -> Option<String> {
        url.as_ref().and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.into_owned())
        })
    };
    let min_axes: i64 = param("min_axes").and_then(|s| s.parse().ok()).unwrap_or(2);
    let min_len: i64 = param("min_len").and_then(|s| s.parse().ok()).unwrap_or(800);
    let limit: usize = param("limit").and_then(|s| s.parse().ok()).unwrap_or(20);

    let db = env.d1("DB")?;
    let result: std::result::Result<serde_json::Value, (u16, String)> = worker_auth_ctx::AUTH_CTX
        .scope(auth, async move {
            worker_auth_ctx::check_scope(&db, "encode")
                .await
                .map_err(|e| (403u16, e))?;
            worker_adas::split_judge_preview(env, &db, min_axes, min_len, limit)
                .await
                .map_err(|e| (500u16, format!("split judge: {:?}", e)))
        })
        .await;

    match result {
        Ok(v) => {
            let mut resp = Response::ok(v.to_string())?;
            resp.headers_mut().set("content-type", "application/json")?;
            Ok(resp)
        }
        Err((code, msg)) => Response::error(msg, code),
    }
}

/// GET /admin/split-preview?min_axes=2&min_len=800&limit=50 — ADAS conflation
/// detector preview (CLA-134). READ-ONLY, no Haiku, no writes: ranks live
/// semantics by the over-creation signal (distinct orientation axes fed) + char
/// length, returns the distribution + candidates so the split detector can be
/// calibrated before separation is built. Gated on `encode`.
async fn admin_split_preview_endpoint(env: &Env, req: &Request) -> Result<Response> {
    let bearer = req
        .headers()
        .get("authorization")?
        .and_then(|h| h.strip_prefix("Bearer ").map(|s| s.to_string()));
    let Some(bearer) = bearer else {
        return Response::error("Missing Authorization: Bearer <key>", 401);
    };
    let Some(auth) = worker_auth_ctx::validate_bearer(env, &bearer).await else {
        return Response::error("Invalid or unknown bearer token", 401);
    };

    let url = req.url().ok();
    let param = |key: &str| -> Option<String> {
        url.as_ref().and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.into_owned())
        })
    };
    let min_axes: i64 = param("min_axes").and_then(|s| s.parse().ok()).unwrap_or(2);
    let min_len: i64 = param("min_len").and_then(|s| s.parse().ok()).unwrap_or(800);
    let limit: usize = param("limit").and_then(|s| s.parse().ok()).unwrap_or(50);

    let db = env.d1("DB")?;
    let result: std::result::Result<serde_json::Value, (u16, String)> = worker_auth_ctx::AUTH_CTX
        .scope(auth, async move {
            worker_auth_ctx::check_scope(&db, "encode")
                .await
                .map_err(|e| (403u16, e))?;
            worker_adas::split_candidate_preview(&db, min_axes, min_len, limit)
                .await
                .map_err(|e| (500u16, format!("split preview: {:?}", e)))
        })
        .await;

    match result {
        Ok(v) => {
            let mut resp = Response::ok(v.to_string())?;
            resp.headers_mut().set("content-type", "application/json")?;
            Ok(resp)
        }
        Err((code, msg)) => Response::error(msg, code),
    }
}

/// GET /admin/cscc-execute?threshold=0.90&min_size=2&commit=false — CSCC merge
/// executor (CLA-134). Clusters + judges like cscc-calibrate, then for each MERGE
/// verdict folds the non-keeper members into the keeper (repoint lineage edges,
/// supersede the folded). `commit=false` (default) is a DRY RUN — no writes, just
/// the fold plan. `commit=true` writes. Gated on `encode`. A store backup before
/// the first real commit is prudent; folds are reversible (superseded rows kept).
async fn admin_cscc_execute_endpoint(env: &Env, req: &Request) -> Result<Response> {
    let bearer = req
        .headers()
        .get("authorization")?
        .and_then(|h| h.strip_prefix("Bearer ").map(|s| s.to_string()));
    let Some(bearer) = bearer else {
        return Response::error("Missing Authorization: Bearer <key>", 401);
    };
    let Some(auth) = worker_auth_ctx::validate_bearer(env, &bearer).await else {
        return Response::error("Invalid or unknown bearer token", 401);
    };

    let url = req.url().ok();
    let param = |key: &str| -> Option<String> {
        url.as_ref().and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.into_owned())
        })
    };
    let threshold: f64 = param("threshold")
        .and_then(|s| s.parse().ok())
        .unwrap_or(worker_cscc::DEFAULT_THRESHOLD);
    let min_size: usize = param("min_size").and_then(|s| s.parse().ok()).unwrap_or(2);
    let commit: bool = param("commit")
        .map(|s| s == "true" || s == "1")
        .unwrap_or(false);
    let use_cache: bool = !param("nocache").map(|s| s == "true" || s == "1").unwrap_or(false);

    let db = env.d1("DB")?;
    let result: std::result::Result<serde_json::Value, (u16, String)> = worker_auth_ctx::AUTH_CTX
        .scope(auth, async move {
            worker_auth_ctx::check_scope(&db, "encode")
                .await
                .map_err(|e| (403u16, e))?;
            worker_cscc::execute_merges(env, &db, threshold, min_size, commit, use_cache)
                .await
                .map_err(|e| (500u16, format!("cscc execute: {:?}", e)))
        })
        .await;

    match result {
        Ok(v) => {
            let mut resp = Response::ok(v.to_string())?;
            resp.headers_mut().set("content-type", "application/json")?;
            Ok(resp)
        }
        Err((code, msg)) => Response::error(msg, code),
    }
}

/// GET /admin/cscc-calibrate?threshold=0.90&min_size=2 — DRY-RUN merge calibrator
/// (CLA-134). Clusters the live semantic layer by cosine proximity (same path as
/// cluster-preview) and runs each cluster through a Haiku merge/keep-distinct
/// judge. NO writes — returns the verdicts + reasons so the merge judgment can be
/// tuned on real output before any execution plumbing is wired. Gated on `encode`.
async fn admin_cscc_calibrate_endpoint(env: &Env, req: &Request) -> Result<Response> {
    let bearer = req
        .headers()
        .get("authorization")?
        .and_then(|h| h.strip_prefix("Bearer ").map(|s| s.to_string()));
    let Some(bearer) = bearer else {
        return Response::error("Missing Authorization: Bearer <key>", 401);
    };
    let Some(auth) = worker_auth_ctx::validate_bearer(env, &bearer).await else {
        return Response::error("Invalid or unknown bearer token", 401);
    };

    let url = req.url().ok();
    let param = |key: &str| -> Option<String> {
        url.as_ref().and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.into_owned())
        })
    };
    let threshold: f64 = param("threshold")
        .and_then(|s| s.parse().ok())
        .unwrap_or(worker_cscc::DEFAULT_THRESHOLD);
    let min_size: usize = param("min_size").and_then(|s| s.parse().ok()).unwrap_or(2);
    let use_cache: bool = !param("nocache").map(|s| s == "true" || s == "1").unwrap_or(false);

    let db = env.d1("DB")?;
    let result: std::result::Result<serde_json::Value, (u16, String)> = worker_auth_ctx::AUTH_CTX
        .scope(auth, async move {
            worker_auth_ctx::check_scope(&db, "encode")
                .await
                .map_err(|e| (403u16, e))?;
            worker_cscc::calibrate(env, &db, threshold, min_size, use_cache)
                .await
                .map_err(|e| (500u16, format!("cscc calibrate: {:?}", e)))
        })
        .await;

    match result {
        Ok(v) => {
            let mut resp = Response::ok(v.to_string())?;
            resp.headers_mut().set("content-type", "application/json")?;
            Ok(resp)
        }
        Err((code, msg)) => Response::error(msg, code),
    }
}

/// GET /admin/cluster-preview?threshold=0.95&min_size=2 — read-only proximity
/// clustering of the live semantic layer (CLA-134). NO Haiku, NO writes: pulls
/// every non-superseded semantic's vector from Vectorize, computes exact pairwise
/// cosine in-Worker, union-finds the pairs at/above `threshold` into clusters, and
/// returns each cluster with its members and intra-cluster min/max/avg similarity.
/// The min is the weakest link — a low min on a big cluster means it's chained
/// through transitivity, not genuinely tight. Lets us calibrate the consolidation
/// similarity threshold against the real store before building the (expensive,
/// write-bearing) REM proximity-consolidation pass. `threshold` is cosine
/// similarity (higher = tighter; 0.95 ≈ distance 0.05). Gated on `encode`.
async fn admin_cluster_preview_endpoint(env: &Env, req: &Request) -> Result<Response> {
    let bearer = req
        .headers()
        .get("authorization")?
        .and_then(|h| h.strip_prefix("Bearer ").map(|s| s.to_string()));
    let Some(bearer) = bearer else {
        return Response::error("Missing Authorization: Bearer <key>", 401);
    };
    let Some(auth) = worker_auth_ctx::validate_bearer(env, &bearer).await else {
        return Response::error("Invalid or unknown bearer token", 401);
    };

    let url = req.url().ok();
    let param = |key: &str| -> Option<String> {
        url.as_ref().and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.into_owned())
        })
    };
    let threshold: f64 = param("threshold").and_then(|s| s.parse().ok()).unwrap_or(0.95);
    let min_size: usize = param("min_size").and_then(|s| s.parse().ok()).unwrap_or(2);

    let db = env.d1("DB")?;
    let result: std::result::Result<serde_json::Value, (u16, String)> = worker_auth_ctx::AUTH_CTX
        .scope(auth, async move {
            worker_auth_ctx::check_scope(&db, "encode")
                .await
                .map_err(|e| (403u16, e))?;

            let semantics = worker_store::all_live_semantics(&db)
                .await
                .map_err(|e| (500u16, format!("list semantics: {:?}", e)))?;

            // Pull every semantic's vector from Vectorize (chunked ≤20 — the
            // binding errors above that). getByIds silently drops ids it
            // doesn't hold, so build a map and count the misses.
            let mut vecs: std::collections::HashMap<String, Vec<f64>> =
                std::collections::HashMap::new();
            for chunk in semantics.chunks(20) {
                let ids: Vec<&str> = chunk.iter().map(|s| s.id.as_str()).collect();
                let fetched = worker_vectorize::get_by_ids(env, &ids)
                    .await
                    .map_err(|e| (500u16, format!("get_by_ids: {:?}", e)))?;
                for sv in fetched {
                    vecs.insert(sv.id, sv.values);
                }
            }

            // Cluster only semantics that actually resolved to a vector.
            let nodes: Vec<&worker_store::SemanticBrief> =
                semantics.iter().filter(|s| vecs.contains_key(&s.id)).collect();
            let n = nodes.len();
            let missing = semantics.len() - n;

            // Union-find over pairs at/above threshold. O(n²) cosine — trivial
            // at a few hundred nodes. We keep each qualifying pair's score so
            // we can report per-cluster min/max/avg afterwards.
            fn find(parent: &mut [usize], mut x: usize) -> usize {
                while parent[x] != x {
                    parent[x] = parent[parent[x]];
                    x = parent[x];
                }
                x
            }
            let mut parent: Vec<usize> = (0..n).collect();
            let mut pair_scores: Vec<(usize, usize, f64)> = Vec::new();
            for i in 0..n {
                let vi = &vecs[&nodes[i].id];
                for j in (i + 1)..n {
                    let vj = &vecs[&nodes[j].id];
                    let sim = hybrid::cosine_similarity(vi, vj);
                    if sim >= threshold {
                        pair_scores.push((i, j, sim));
                        let ri = find(&mut parent, i);
                        let rj = find(&mut parent, j);
                        if ri != rj {
                            parent[ri] = rj;
                        }
                    }
                }
            }

            // Group node indices by their union-find root.
            let mut groups: std::collections::HashMap<usize, Vec<usize>> =
                std::collections::HashMap::new();
            for i in 0..n {
                let r = find(&mut parent, i);
                groups.entry(r).or_default().push(i);
            }
            let round3 = |x: f64| (x * 1000.0).round() / 1000.0;

            let mut clusters: Vec<serde_json::Value> = Vec::new();
            for members in groups.values() {
                if members.len() < min_size {
                    continue;
                }
                let mset: std::collections::HashSet<usize> = members.iter().copied().collect();
                let intra: Vec<f64> = pair_scores
                    .iter()
                    .filter(|(a, b, _)| mset.contains(a) && mset.contains(b))
                    .map(|(_, _, s)| *s)
                    .collect();
                let (min_s, max_s, avg_s) = if intra.is_empty() {
                    (0.0, 0.0, 0.0)
                } else {
                    let mn = intra.iter().cloned().fold(f64::INFINITY, f64::min);
                    let mx = intra.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                    let av = intra.iter().sum::<f64>() / intra.len() as f64;
                    (mn, mx, av)
                };
                // Strongest member first — the one most likely to be the keeper.
                let mut mems: Vec<&worker_store::SemanticBrief> =
                    members.iter().map(|&i| nodes[i]).collect();
                mems.sort_by(|a, b| {
                    b.strength
                        .partial_cmp(&a.strength)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                let member_json: Vec<serde_json::Value> = mems
                    .iter()
                    .map(|m| {
                        serde_json::json!({
                            "id": &m.id[..m.id.len().min(8)],
                            "strength": round3(m.strength),
                            "summary": m.summary,
                        })
                    })
                    .collect();
                clusters.push(serde_json::json!({
                    "size": members.len(),
                    "min_sim": round3(min_s),
                    "max_sim": round3(max_s),
                    "avg_sim": round3(avg_s),
                    "members": member_json,
                }));
            }
            // Biggest clusters first — those are where the calibration signal is.
            clusters.sort_by(|a, b| {
                b["size"].as_u64().unwrap_or(0).cmp(&a["size"].as_u64().unwrap_or(0))
            });

            let clustered: usize = clusters
                .iter()
                .map(|c| c["size"].as_u64().unwrap_or(0) as usize)
                .sum();
            Ok(serde_json::json!({
                "threshold": threshold,
                "min_size": min_size,
                "semantics_total": semantics.len(),
                "with_vector": n,
                "missing_vector": missing,
                "qualifying_pairs": pair_scores.len(),
                "clusters": clusters.len(),
                "clustered_semantics": clustered,
                "detail": clusters,
            }))
        })
        .await;

    match result {
        Ok(v) => {
            let mut resp = Response::ok(v.to_string())?;
            resp.headers_mut().set("content-type", "application/json")?;
            Ok(resp)
        }
        Err((code, msg)) => Response::error(msg, code),
    }
}

/// GET /admin/decompose-preview?episodic_id=<id|prefix> — read-only Step 0 of the
/// encode rebuild (CLA-134). Decomposes one episodic into its atomic knowledge-
/// units and returns them; NO judge, NO writes. The point is to eyeball whether
/// the atomisation granularity is right (on the known whiff `03d4770a`) before we
/// build the per-unit judge — same calibrate-first play as cluster-preview. The
/// decompose call is instrumented, so a decompose-side truncation also lands in
/// `encode_diagnostics` (source=decompose). Gated on `encode`.
async fn admin_decompose_preview_endpoint(env: &Env, req: &Request) -> Result<Response> {
    let bearer = req
        .headers()
        .get("authorization")?
        .and_then(|h| h.strip_prefix("Bearer ").map(|s| s.to_string()));
    let Some(bearer) = bearer else {
        return Response::error("Missing Authorization: Bearer <key>", 401);
    };
    let Some(auth) = worker_auth_ctx::validate_bearer(env, &bearer).await else {
        return Response::error("Invalid or unknown bearer token", 401);
    };

    let episodic_id = req.url().ok().and_then(|u| {
        u.query_pairs()
            .find(|(k, _)| k == "episodic_id")
            .map(|(_, v)| v.into_owned())
    });
    let Some(episodic_id) = episodic_id else {
        return Response::error("Missing ?episodic_id=<id or 8-char prefix>", 400);
    };

    let db = env.d1("DB")?;
    let result: std::result::Result<serde_json::Value, (u16, String)> = worker_auth_ctx::AUTH_CTX
        .scope(auth, async move {
            worker_auth_ctx::check_scope(&db, "encode")
                .await
                .map_err(|e| (403u16, e))?;

            let ep = worker_store::find_by_prefix(&db, &episodic_id)
                .await
                .map_err(|e| (500u16, format!("find episodic: {:?}", e)))?
                .ok_or((404u16, format!("episodic {} not found", episodic_id)))?;

            let units = worker_encode::decompose(env, &db, &ep)
                .await
                .map_err(|e| (500u16, format!("decompose: {:?}", e)))?;

            Ok(serde_json::json!({
                "episodic": &ep.id[..ep.id.len().min(8)],
                "chars": ep.content.chars().count(),
                "unit_count": units.len(),
                "units": units,
            }))
        })
        .await;

    match result {
        Ok(v) => {
            let mut resp = Response::ok(v.to_string())?;
            resp.headers_mut().set("content-type", "application/json")?;
            Ok(resp)
        }
        Err((code, msg)) => Response::error(msg, code),
    }
}

/// POST /admin/encode-decomposed?episodic_id=<id|prefix>&commit=<bool> — Step 1
/// of the encode rebuild (CLA-134). decompose → per-unit candidate search →
/// per-unit single-decision judge → filings. `commit` defaults to **false** (dry
/// run: judge everything, write nothing) so the judge's filing choices can be
/// eyeballed on the known whiff `03d4770a` before any write to the sacred store;
/// `commit=true` dispatches. Success on the dry run = the episodic that filed 0
/// in the monolith now yields a sane filing per unit. Gated on `encode`.
async fn admin_encode_decomposed_endpoint(env: &Env, req: &Request) -> Result<Response> {
    let bearer = req
        .headers()
        .get("authorization")?
        .and_then(|h| h.strip_prefix("Bearer ").map(|s| s.to_string()));
    let Some(bearer) = bearer else {
        return Response::error("Missing Authorization: Bearer <key>", 401);
    };
    let Some(auth) = worker_auth_ctx::validate_bearer(env, &bearer).await else {
        return Response::error("Invalid or unknown bearer token", 401);
    };

    let url = req.url().ok();
    let param = |key: &str| -> Option<String> {
        url.as_ref().and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.into_owned())
        })
    };
    let Some(episodic_id) = param("episodic_id") else {
        return Response::error("Missing ?episodic_id=<id or 8-char prefix>", 400);
    };
    let commit = param("commit").as_deref() == Some("true");
    let limit: Option<usize> = param("limit").and_then(|s| s.parse().ok());

    let db = env.d1("DB")?;
    let result: std::result::Result<serde_json::Value, (u16, String)> = worker_auth_ctx::AUTH_CTX
        .scope(auth, async move {
            worker_auth_ctx::check_scope(&db, "encode")
                .await
                .map_err(|e| (403u16, e))?;

            let ep = worker_store::find_by_prefix(&db, &episodic_id)
                .await
                .map_err(|e| (500u16, format!("find episodic: {:?}", e)))?
                .ok_or((404u16, format!("episodic {} not found", episodic_id)))?;

            let filings = worker_encode::encode_decomposed(env, &db, &ep, commit, limit)
                .await
                .map_err(|e| (500u16, format!("encode_decomposed: {:?}", e)))?;

            let actions: std::collections::HashMap<&str, usize> =
                filings.iter().fold(std::collections::HashMap::new(), |mut m, f| {
                    *m.entry(f.action.as_str()).or_insert(0) += 1;
                    m
                });
            Ok(serde_json::json!({
                "episodic": &ep.id[..ep.id.len().min(8)],
                "committed": commit,
                "unit_count": filings.len(),
                "actions": actions,
                "filings": filings,
            }))
        })
        .await;

    match result {
        Ok(v) => {
            let mut resp = Response::ok(v.to_string())?;
            resp.headers_mut().set("content-type", "application/json")?;
            Ok(resp)
        }
        Err((code, msg)) => Response::error(msg, code),
    }
}

/// GET /authorize — renders the consent HTML page. Query params:
///   client_id, redirect_uri, state, scope, code_challenge
///
/// CLA-91 Fix 2 — redirect_uri validated against the allowlist before
/// the page renders. Unregistered URIs get a 400 instead of a primed
/// consent page that would later create a pending code for an exfil
/// destination.
async fn render_consent_page(env: &Env, req: &Request) -> Result<Response> {
    let url = req.url()?;
    let q = |key: &str| -> String {
        url.query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default()
    };
    let redirect_uri = q("redirect_uri");
    if !worker_oauth::is_registered_redirect_uri(env, &redirect_uri).await {
        // Include the offending URI in the response body so the operator
        // can copy it directly into ONEIRO_OAUTH_REDIRECT_URIS without
        // having to dig it out of logs or guess at form-encoded params.
        // (Setup script's troubleshooting hint relies on this text.)
        return Response::error(
            format!("invalid_request: redirect_uri not registered: {}", redirect_uri),
            400,
        );
    }
    worker_oauth::render_authorize_page(
        &q("client_id"),
        &redirect_uri,
        &q("state"),
        &q("scope"),
        &q("code_challenge"),
    )
}

async fn handle_authorize_form(env: &Env, req: &mut Request) -> Result<Response> {
    let body = req.text().await?;
    let form = worker_oauth::parse_form(&body);
    worker_oauth::handle_authorize_post(env, form).await
}

async fn handle_token_form(env: &Env, req: &mut Request) -> Result<Response> {
    let body = req.text().await?;
    let form = worker_oauth::parse_form(&body);
    worker_oauth::handle_token_post(env, form).await
}

/// Scheduled handler — dispatched by cron triggers declared in
/// wrangler.toml. We use the cron pattern that fired (via `event.cron()`)
/// to decide which cognitive loop to invoke:
///
///   - `0 14 * * *` (14:00 UTC / 00:00 AEST) → REM consolidator
///   - `0 8 * * *`  (08:00 UTC / 18:00 AEST) → Dialectic (CLA-95)
///
/// Unknown cron patterns are deliberately not dispatched — they log an
/// error and no-op. Per CLA-95 PR #7 review: silent-and-mostly-fine
/// (fall through to REM) is worse than visible-and-wrong (log + no-op)
/// when a typo'd cron entry hits production. Adding a new cron requires
/// adding a match arm here.
#[event(scheduled)]
pub async fn scheduled(event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    let cron = event.cron();
    match cron.as_str() {
        "0 8 * * *" => run_dialectic(&env).await,
        "0 14 * * *" => run_rem(&env).await,
        other => {
            worker::console_error!(
                "Unknown cron trigger: {} — no handler dispatched",
                other
            );
        }
    }
}

/// Queue consumer — the capture pipeline (write episodic → encode to semantics).
/// Producers are the `/encode` hook endpoint and the `reflect` MCP tool, both
/// available on every client. Decouples a fast tool response from the slow Haiku
/// encode; each message acks on success or retries (landing in the DLQ after
/// max_retries) so nothing is silently lost. Orientation distillation is a
/// separate nightly cron — this handles only episodic → semantic.
#[event(queue)]
pub async fn queue(
    batch: MessageBatch<serde_json::Value>,
    env: Env,
    _ctx: Context,
) -> Result<()> {
    let db = env.d1("DB")?;
    // worker 0.8 exposes only batch-level ack/retry. With max_batch_size = 1
    // (wrangler.toml), retry_all can never re-deliver an already-succeeded
    // sibling — which would duplicate its episodic, since create is not
    // idempotent. One message per batch keeps retry safe.
    for message in batch.messages()? {
        // Lenient decode: a tagged QueueMessage, or — for any bare CaptureMessage
        // still in flight from before this deploy — fall back to a Capture, so the
        // message-format change can't strand queued captures in the DLQ.
        let raw = message.body();
        let parsed = serde_json::from_value::<worker_encode::QueueMessage>(raw.clone()).or_else(|_| {
            serde_json::from_value::<worker_encode::CaptureMessage>(raw.clone())
                .map(worker_encode::QueueMessage::Capture)
        });
        let msg = match parsed {
            Ok(m) => m,
            Err(e) => {
                worker::console_error!("queue: undecodable message ({:?}), retrying", e);
                batch.retry_all();
                return Ok(());
            }
        };
        if let Err(e) = handle_queue_message(&env, &db, msg).await {
            // Two propagating cases, both retry-safe: a Capture write failure
            // (episodic NOT written → no duplicate), or an EncodeUnit judge/
            // dispatch failure (retries that one unit, never re-writes the
            // episodic; a re-judged duplicate semantic is merged by CSCC).
            worker::console_error!("queue message failed, retrying: {:?}", e);
            batch.retry_all();
            return Ok(());
        }
    }
    batch.ack_all();
    Ok(())
}

/// First poll fires this many seconds after submit; each subsequent poll
/// re-enqueues at the same delay. The batch floor is ~190s, so the first poll or
/// two usually find it still processing and re-enqueue.
const POLL_DELAY_SECONDS: u32 = 90;
/// Give up after this many polls (~2h at 90s/poll) and record a failure for the
/// manual catch-up — comfortably beyond a typical sub-hour batch completion.
const MAX_POLL_ATTEMPTS: u32 = 80;

/// Dispatch one queue message. Propagates `Err` only when a retry is safe: a
/// Capture write failure (no episodic written yet), or an EncodeUnit judge/
/// dispatch failure (retries that unit, never re-writing the episodic). Work
/// after a Capture's episodic IS written (decompose + fan-out) is best-effort
/// and never propagates, because a retry would re-write the episodic.
async fn handle_queue_message(
    env: &Env,
    db: &D1Database,
    msg: worker_encode::QueueMessage,
) -> Result<()> {
    match msg {
        worker_encode::QueueMessage::Capture(cap) => {
            let ep = worker_encode::process_capture(db, &cap).await?; // only this propagates
            let short = ep.id[..ep.id.len().min(8)].to_string();
            // Decompose-first encode (CLA-134): atomise the episodic in one small
            // call, then fan each unit out as its own EncodeUnit message so no
            // single invocation runs the whole 18–30-call encode (the wall-time
            // wall the old batch path was built around). Best-effort: the episodic
            // is durably written, so a failure here is logged, not propagated — a
            // retry would re-write the episodic. Unintegrated episodics are
            // re-encoded via the manual backlog (/admin/encode-decomposed).
            match worker_encode::decompose_and_fan_out(env, db, &ep).await {
                Ok(n) => worker::console_log!("capture {} → {} units fanned out", short, n),
                Err(e) => worker::console_error!(
                    "decompose/fan-out after capture failed for {} ({:?}) — left for catch-up",
                    short,
                    e
                ),
            }
            Ok(())
        }
        worker_encode::QueueMessage::EncodeUnit { episodic_id, unit } => {
            // Judge + dispatch one atomic unit — a single small call, safely
            // within one invocation. Propagates on error so the queue retries
            // THIS unit (a re-judge may duplicate a semantic, which CSCC merges —
            // over is safe; it never re-writes the episodic).
            let short = episodic_id[..episodic_id.len().min(8)].to_string();
            match worker_store::get(db, &episodic_id).await? {
                Some(ep) => worker_encode::encode_unit(env, db, &ep, &unit).await?,
                None => worker::console_error!("encode-unit: episodic {} gone, skipping", short),
            }
            Ok(())
        }
        worker_encode::QueueMessage::PollBatch { batch_id, attempt } => {
            // Never propagate — a retry would double-enqueue the poll. Resilience
            // is the delayed re-poll itself: on a transient error, re-enqueue.
            if let Err(e) = poll_and_reschedule(env, db, &batch_id, attempt).await {
                worker::console_error!("poll {} failed ({:?}); re-enqueueing", batch_id, e);
                let _ = enqueue_poll(env, &batch_id, attempt + 1).await;
            }
            Ok(())
        }
    }
}

/// Enqueue a delayed poll of `batch_id` — the self-perpetuating poll loop.
async fn enqueue_poll(env: &Env, batch_id: &str, attempt: u32) -> Result<()> {
    let msg = worker_encode::QueueMessage::PollBatch {
        batch_id: batch_id.to_string(),
        attempt,
    };
    env.queue("CAPTURE_QUEUE")?
        .send(MessageBuilder::new(msg).delay_seconds(POLL_DELAY_SECONDS).build())
        .await
}

/// Poll one in-flight batch; dispatch + finish if ended, else re-enqueue. Gives
/// up past `MAX_POLL_ATTEMPTS`, recording a failure row per episodic.
async fn poll_and_reschedule(
    env: &Env,
    db: &D1Database,
    batch_id: &str,
    attempt: u32,
) -> Result<()> {
    let batch = match worker_store::in_progress_encode_batches(db)
        .await?
        .into_iter()
        .find(|b| b.batch_id == batch_id)
    {
        Some(b) => b,
        // Already dispatched or failed by an earlier poll — nothing to do.
        None => return Ok(()),
    };

    if attempt >= MAX_POLL_ATTEMPTS {
        for ep in &batch.episodic_ids {
            let _ = worker_store::record_encode_failure(db, ep, batch_id, "poll_exhausted").await;
        }
        worker_store::set_encode_batch_status(db, batch_id, "failed").await?;
        worker::console_error!("encode-batch {} gave up after {} polls", batch_id, attempt);
        return Ok(());
    }

    match worker_encode_batch::poll_batch(env, db, &batch).await? {
        worker_encode_batch::PollOutcome::Done => Ok(()),
        worker_encode_batch::PollOutcome::StillWaiting => {
            enqueue_poll(env, batch_id, attempt + 1).await
        }
    }
}

async fn run_rem(env: &Env) {
    match worker_rem::run(env).await {
        Ok(summary) => {
            worker::console_log!(
                "REM run complete: decayed={} clusters={} created={} appended={} revised={} skipped={} errors={}",
                summary.decayed,
                summary.clusters_attempted,
                summary.decisions_created,
                summary.decisions_appended,
                summary.decisions_revised,
                summary.decisions_skipped,
                summary.errors.len()
            );
            for err in &summary.errors {
                worker::console_error!("REM partial error: {}", err);
            }
        }
        Err(e) => {
            worker::console_error!("REM run failed catastrophically: {:?}", e);
        }
    }
}

async fn run_dialectic(env: &Env) {
    match worker_dialectic::run(env).await {
        Ok(summary) => {
            worker::console_log!(
                "Dialectic run complete: candidates={} decisions={} errors={}",
                summary.candidates_reviewed,
                summary.decisions_count,
                summary.errors.len()
            );
            for err in &summary.errors {
                worker::console_error!("Dialectic partial error: {}", err);
            }
        }
        Err(e) => {
            worker::console_error!("Dialectic run failed catastrophically: {:?}", e);
        }
    }
}
