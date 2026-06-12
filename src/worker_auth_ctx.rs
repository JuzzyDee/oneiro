// worker_auth_ctx.rs — Per-request auth context for the wasm32 worker.
//
// Mirrors the native auth_ctx.rs pattern: a tokio task-local carries the
// authenticated caller through the MCP dispatch chain so tool handlers
// can scope-gate, audit, and entity-bind without each handler re-doing
// the bearer check. Phase 5a (this commit) covers the service API key
// path; Phase 5b will add OAuth bearer resolution against KV.
//
// The check_scope() function enforces three gates in order — scope
// allowlist, rate limit, audit row — matching CLA-86's design exactly.

use crate::api_key::{self, ApiKeyEntry, Role};
use crate::audit;
use crate::key_rate;
use crate::worker_audit;
use std::sync::OnceLock;
use worker::{D1Database, Env};

/// The auth context for the currently-handled MCP request.
#[derive(Debug, Clone)]
pub enum AuthCtx {
    /// OAuth bearer (full access). Phase 5b implements the resolution.
    OAuth,
    /// Service API key — role + stable key_id (sha256 prefix of the raw).
    ApiKey { role: Role, key_id: String },
}

tokio::task_local! {
    pub static AUTH_CTX: AuthCtx;
}

/// Read tools — for rate-limit bucketing. Updated CLA-103: `recall`
/// retired in favour of `recall_orient`; `review` retired from MCP and
/// moved to a direct worker_store call by the dialectic.
const READ_TOOLS: &[&str] = &[
    "recall_orient",
    "recall_check",
    "recall_specific",
    "recall_image",
];
/// Write tools — separate (smaller) rate budget.
const WRITE_TOOLS: &[&str] = &["remember", "remember_with_image", "encode"];

fn is_write_tool(tool: &str) -> Option<bool> {
    if READ_TOOLS.contains(&tool) {
        Some(false)
    } else if WRITE_TOOLS.contains(&tool) {
        Some(true)
    } else {
        None
    }
}

/// Provenance label for `Memory.recorded_by` based on the current
/// AUTH_CTX. None when AUTH_CTX is unset (which shouldn't happen on the
/// HTTP path, but provides a safe default).
pub fn current_recorded_by() -> Option<String> {
    AUTH_CTX
        .try_with(|ctx| match ctx {
            AuthCtx::OAuth => Some("claude".to_string()),
            AuthCtx::ApiKey { role, .. } => Some(role.as_str().to_string()),
        })
        .unwrap_or(None)
}

/// Cache the parsed API key entries across requests. ONEIRO_API_KEYS is
/// read from the worker's env (secret) on first access; subsequent
/// requests reuse the parsed list.
static API_KEY_ENTRIES: OnceLock<Vec<ApiKeyEntry>> = OnceLock::new();

fn entries(env: &Env) -> &'static [ApiKeyEntry] {
    API_KEY_ENTRIES.get_or_init(|| {
        // Try secret first (production), then env var (dev).
        let raw = env
            .secret("ONEIRO_API_KEYS")
            .map(|s| s.to_string())
            .or_else(|_| env.var("ONEIRO_API_KEYS").map(|v| v.to_string()))
            .unwrap_or_default();
        if raw.trim().is_empty() {
            return Vec::new();
        }
        // Lean on api_key::load_from_env-style parsing — but we have the
        // raw string already, not the env var name. Pull the same parser
        // out by re-using the same internal logic via load_from_env's
        // public surface... actually load_from_env reads std::env, which
        // returns empty on wasm. So duplicate the small parse here.
        parse_inline(&raw)
    })
}

/// Inline parse of `<role>:<hash>;<role>:<hash>` — same shape as the
/// native `ONEIRO_API_KEYS` parser in api_key::load_from_env. Kept here
/// (rather than reaching into api_key's private parse_entries) so we
/// don't have to widen its visibility.
fn parse_inline(raw: &str) -> Vec<ApiKeyEntry> {
    raw.split(';')
        .filter_map(|segment| {
            let segment = segment.trim();
            if segment.is_empty() {
                return None;
            }
            let (role_str, hash) = segment.split_once(':')?;
            let role = Role::from_str(role_str.trim())?;
            // Validate the hash parses as Argon2 PHC — bad rows are skipped
            // with a warning rather than failing all auth.
            if argon2::PasswordHash::new(hash.trim()).is_err() {
                tracing::warn!(
                    "ONEIRO_API_KEYS contains an invalid argon2 hash; skipping entry"
                );
                return None;
            }
            Some(ApiKeyEntry {
                role,
                hash: hash.trim().to_string(),
            })
        })
        .collect()
}

/// Validate a Bearer token. Two paths:
///   1. OAuth — `mem_<hex>` format, looked up in KV.
///   2. Service API key — `mk_<role>_<rand>` format, argon2-verified
///      against ONEIRO_API_KEYS.
/// The OAuth check is async (KV lookup); the API key check is sync.
pub async fn validate_bearer(env: &Env, bearer: &str) -> Option<AuthCtx> {
    if crate::worker_oauth::looks_like_oauth_token(bearer) {
        if let Ok(true) = crate::worker_oauth::validate_token(env, bearer).await {
            return Some(AuthCtx::OAuth);
        }
        // OAuth lookup failed — don't fall through to API key path,
        // returning None here lets the caller send 401.
        return None;
    }
    let entries = entries(env);
    let auth = api_key::verify_api_key(bearer, entries)?;
    Some(AuthCtx::ApiKey {
        role: auth.role,
        key_id: auth.key_id,
    })
}

/// Three-gate scope check: allowlist, rate, audit. Same shape as the
/// native auth_ctx::check_scope; the only difference is that audit
/// writes to D1 (async, via worker_audit::log) instead of SQLite.
pub async fn check_scope(db: &D1Database, tool: &str) -> Result<(), String> {
    let Some(ctx) = AUTH_CTX.try_with(|c| c.clone()).ok() else {
        // No AUTH_CTX set — treat as local-trust (matches stdio path).
        return Ok(());
    };

    match ctx {
        AuthCtx::OAuth => Ok(()),
        AuthCtx::ApiKey { role, ref key_id } => {
            // Gate 1: scope
            if !role.allows(tool) {
                worker_audit::log(
                    db,
                    audit::AuditEntry {
                        key_id,
                        role,
                        tool,
                        outcome: audit::Outcome::ScopeViolation,
                    },
                )
                .await;
                tracing::warn!(
                    "Scope violation: role={} key_id={} tool=`{}`",
                    role.as_str(),
                    key_id,
                    tool
                );
                return Err(format!(
                    "Forbidden: role `{}` is not permitted to call `{}`",
                    role.as_str(),
                    tool
                ));
            }

            // Gate 2: rate limit (read/write classified tools only)
            if let Some(is_write) = is_write_tool(tool) {
                if key_rate::global().check_and_count(key_id, is_write).is_err() {
                    worker_audit::log(
                        db,
                        audit::AuditEntry {
                            key_id,
                            role,
                            tool,
                            outcome: audit::Outcome::RateLimited,
                        },
                    )
                    .await;
                    tracing::warn!(
                        "Rate limited: role={} key_id={} tool=`{}` is_write={}",
                        role.as_str(),
                        key_id,
                        tool,
                        is_write
                    );
                    return Err(format!(
                        "Too many requests: key {} exceeded its {} budget. \
                         Try again shortly.",
                        key_id,
                        if is_write { "write" } else { "read" }
                    ));
                }
            }

            // Success path
            worker_audit::log(
                db,
                audit::AuditEntry {
                    key_id,
                    role,
                    tool,
                    outcome: audit::Outcome::Ok,
                },
            )
            .await;
            Ok(())
        }
    }
}
