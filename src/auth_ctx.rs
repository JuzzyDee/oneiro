// auth_ctx.rs — Per-request auth context, propagated via tokio task-local.
//
// rmcp's tool dispatch sits downstream of the HTTP bearer check. To enforce
// per-role scope gating inside tool handlers, we set a task-local at the
// HTTP boundary right before handing the request to `mcp_svc.handle`, then
// read it inside each handler via `check_scope`.
//
// task-locals propagate through async/await transparently, so any code
// reached from `AUTH_CTX.scope(ctx, fut).await` sees the same context
// regardless of how deep the future tree is.
//
// The HTTP boundary path always sets AUTH_CTX (and DB_PATH for audit
// writes). The stdio path (local mode for Claude Code Desktop) does not
// — when AUTH_CTX is unset, `check_scope` defaults to allowing, which
// matches the historical "local stdio = full trust" stance.

use crate::api_key::Role;
use crate::{audit, key_rate};
use std::path::PathBuf;

/// Tools the rover role may invoke (read side). Used by check_scope to
/// classify a call for rate-limit bucketing — reads draw from a larger
/// budget than writes.
///
/// Updated CLA-103: `recall` retired in favour of `recall_orient`;
/// `review` retired from MCP and moved to a direct worker_store call by
/// the dialectic. Keep in sync with the wasm mirror in
/// `worker_auth_ctx::READ_TOOLS`.
const READ_TOOLS: &[&str] = &[
    "recall_orient",
    "recall_check",
    "recall_specific",
    "recall_image",
];

/// Tools the rover role may invoke (write side).
const WRITE_TOOLS: &[&str] = &["remember", "remember_with_image"];

/// Classify a tool as read (`Some(false)`) or write (`Some(true)`). Returns
/// `None` for tools that aren't on the rover allowlist — those would have
/// been rejected by the scope check before this is consulted.
fn is_write_tool(tool: &str) -> Option<bool> {
    if READ_TOOLS.contains(&tool) {
        Some(false)
    } else if WRITE_TOOLS.contains(&tool) {
        Some(true)
    } else {
        None
    }
}

/// Per-request authentication context. Set at the HTTP bearer-check
/// boundary, read by tool handlers via [`check_scope`].
#[derive(Debug, Clone)]
pub enum AuthCtx {
    /// Authenticated via OAuth bearer — user-facing client (Claude Code /
    /// Web / iOS). Full tool access.
    OAuth,
    /// Authenticated via service API key. Role determines which tools the
    /// caller may invoke; `key_id` flows to the audit trail (phase 6).
    ApiKey { role: Role, key_id: String },
}

tokio::task_local! {
    /// The auth context for the currently-handled MCP request.
    ///
    /// Always set on the HTTP path. Unset on the stdio path (local mode)
    /// and during tests — `check_scope` treats `unset == allow` for those
    /// cases, which preserves existing local-mode behaviour.
    pub static AUTH_CTX: AuthCtx;

    /// Path to the oneiro DB, used by audit writes. Set alongside
    /// AUTH_CTX at the HTTP boundary. Unset in stdio / tests — audit
    /// writes are silently skipped when unset.
    pub static DB_PATH: PathBuf;
}

fn audit_log_if_possible(key_id: &str, role: Role, tool: &str, outcome: audit::Outcome) {
    let _ = DB_PATH.try_with(|path| {
        audit::log(
            path,
            audit::AuditEntry {
                key_id,
                role,
                tool,
                outcome,
            },
        );
    });
}

/// Return the value oneiro should record as a memory's `recorded_by`
/// based on the current auth context.
///
///   `AuthCtx::OAuth`   →  `Some("claude")`
///   `AuthCtx::ApiKey`  →  `Some("<role-name>")` (e.g. `"rover"`)
///   unset (stdio/test) →  `None`             — local trust, legacy-shaped
///
/// `None` is what legacy memories (pre-CLA-86) carry, so stdio writes
/// continue to look identical to pre-migration memories. Remote writes
/// always carry a real source.
///
/// **Forgery defense for tool handlers:** the value returned here is the
/// only acceptable `recorded_by` — callers must use this and ignore any
/// `recorded_by` field in the request body. That's the whole point.
pub fn current_recorded_by() -> Option<String> {
    AUTH_CTX
        .try_with(|ctx| match ctx {
            AuthCtx::OAuth => Some("claude".to_string()),
            AuthCtx::ApiKey { role, .. } => Some(role.as_str().to_string()),
        })
        .unwrap_or(None)
}

/// Check whether the current auth context is permitted to invoke `tool`.
///
/// Three gates, in order:
///   1. **Scope** — the role's allowlist permits the tool (CLA-86 phase 3)
///   2. **Rate** — the key hasn't exceeded its per-minute budget (phase 5)
///   3. **Audit** — every API-key call writes a row capturing the outcome
///                  (phase 6; success or specific failure)
///
/// Returns:
/// - `Ok(())` when OAuth-authenticated (full access, no rate/audit — OAuth
///   is user-facing and scaled differently)
/// - `Ok(())` when API-key role's allowlist permits the tool AND the key
///   is under its rate budget. Audit row written with `Outcome::Ok`.
/// - `Ok(())` when AUTH_CTX is unset (stdio / test path — local trust,
///   no rate, no audit)
/// - `Err(message)` for scope-violation or rate-limited cases. The error
///   string is suitable for returning directly from a tool handler. An
///   audit row is written with the appropriate outcome before returning.
///
/// Audit and rate-limit failures emit a `tracing::warn!` in addition to
/// the audit row.
pub fn check_scope(tool: &str) -> Result<(), String> {
    AUTH_CTX
        .try_with(|ctx| match ctx {
            AuthCtx::OAuth => Ok(()),
            AuthCtx::ApiKey { role, key_id } => {
                // Gate 1: scope
                if !role.allows(tool) {
                    audit_log_if_possible(key_id, *role, tool, audit::Outcome::ScopeViolation);
                    tracing::warn!(
                        "Scope violation: role={} key_id={} attempted to call `{}`",
                        role.as_str(),
                        key_id,
                        tool,
                    );
                    return Err(format!(
                        "Forbidden: role `{}` is not permitted to call `{}`",
                        role.as_str(),
                        tool,
                    ));
                }

                // Gate 2: rate limit (only for classified read/write tools —
                // tools outside both lists must have been gated by scope above,
                // so reaching here with `None` shouldn't happen in practice).
                if let Some(is_write) = is_write_tool(tool) {
                    if key_rate::global().check_and_count(key_id, is_write).is_err() {
                        audit_log_if_possible(key_id, *role, tool, audit::Outcome::RateLimited);
                        tracing::warn!(
                            "Rate limited: role={} key_id={} tool=`{}` is_write={}",
                            role.as_str(),
                            key_id,
                            tool,
                            is_write,
                        );
                        return Err(format!(
                            "Too many requests: key {} exceeded its {} budget. \
                             Try again shortly.",
                            key_id,
                            if is_write { "write" } else { "read" },
                        ));
                    }
                }

                // Success — record the call.
                audit_log_if_possible(key_id, *role, tool, audit::Outcome::Ok);
                Ok(())
            }
        })
        // AUTH_CTX is only set on the HTTP path. stdio mode (Claude Code
        // Desktop local) and unit tests don't set it — treat as full trust,
        // and don't write audit rows for local writes.
        .unwrap_or(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // task-local access requires running inside a tokio runtime; use #[tokio::test].

    #[tokio::test]
    async fn unset_context_allows_everything() {
        // No AUTH_CTX.scope wrapping — should be permissive (stdio / test path).
        assert!(check_scope("recall").is_ok());
        assert!(check_scope("reframe").is_ok());
        assert!(check_scope("forget").is_ok());
        assert!(check_scope("anything_at_all").is_ok());
    }

    #[tokio::test]
    async fn oauth_context_allows_everything() {
        AUTH_CTX
            .scope(AuthCtx::OAuth, async {
                assert!(check_scope("recall").is_ok());
                assert!(check_scope("reframe").is_ok());
                assert!(check_scope("forget").is_ok());
            })
            .await;
    }

    #[tokio::test]
    async fn rover_can_read_and_write_but_not_revise() {
        let ctx = AuthCtx::ApiKey {
            role: Role::Rover,
            key_id: "deadbeef".to_string(),
        };
        AUTH_CTX
            .scope(ctx, async {
                // Allowed (CLA-103: recall→recall_orient, review retired)
                assert!(check_scope("recall_orient").is_ok());
                assert!(check_scope("recall_check").is_ok());
                assert!(check_scope("recall_specific").is_ok());
                assert!(check_scope("recall_image").is_ok());
                assert!(check_scope("remember").is_ok());
                assert!(check_scope("remember_with_image").is_ok());

                // Forbidden — these are the consolidator's job, not the rover's
                assert!(check_scope("reframe").is_err());
                assert!(check_scope("forget").is_err());
                assert!(check_scope("reflect").is_err());

                // Retired in CLA-103 — now reject as unknown tools.
                assert!(check_scope("recall").is_err());
                assert!(check_scope("review").is_err());

                // Unknown tools default to forbidden (matches!(tool, "...") returns false)
                assert!(check_scope("admin_dump_all").is_err());
                assert!(check_scope("").is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn current_recorded_by_unset() {
        assert_eq!(current_recorded_by(), None);
    }

    #[tokio::test]
    async fn current_recorded_by_oauth() {
        AUTH_CTX
            .scope(AuthCtx::OAuth, async {
                assert_eq!(current_recorded_by(), Some("claude".to_string()));
            })
            .await;
    }

    #[tokio::test]
    async fn current_recorded_by_rover() {
        let ctx = AuthCtx::ApiKey {
            role: Role::Rover,
            key_id: "deadbeef".to_string(),
        };
        AUTH_CTX
            .scope(ctx, async {
                assert_eq!(current_recorded_by(), Some("rover".to_string()));
            })
            .await;
    }

    #[tokio::test]
    async fn denial_message_names_role_and_tool() {
        let ctx = AuthCtx::ApiKey {
            role: Role::Rover,
            key_id: "deadbeef".to_string(),
        };
        AUTH_CTX
            .scope(ctx, async {
                let err = check_scope("reframe").unwrap_err();
                assert!(err.contains("rover"));
                assert!(err.contains("reframe"));
                assert!(err.to_lowercase().contains("forbidden"));
            })
            .await;
    }
}
