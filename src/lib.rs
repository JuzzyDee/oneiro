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
mod worker_rem;
mod worker_rem_audit;
mod worker_store;
mod worker_vectorize;
mod worker_version;

use worker::{event, Context, Env, Method, Request, Response, Result, ScheduleContext, ScheduledEvent};

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
    if worker_auth_ctx::validate_bearer(env, &bearer).await.is_none() {
        return Response::error("Invalid or unknown bearer token", 401);
    }

    let db = env.d1("DB")?;
    let orientation = worker_store::get_orientation(&db).await.map_err(|e| {
        worker::Error::RustError(format!("get_orientation: {:?}", e))
    })?;
    let counts = worker_store::count_by_type(&db).await.unwrap_or((0, 0, 0));
    let payload = worker_orient::format_payload(&orientation, None, counts);

    let mut resp = Response::ok(payload)?;
    resp.headers_mut()
        .set("content-type", "text/plain; charset=utf-8")?;
    Ok(resp)
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
