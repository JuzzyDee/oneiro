// worker_lease.rs — single-flight lease for the nightly cognitive-write crons (CLA-134).
//
// Once CSCC / orient-run / dialectic move from manual triggers to cron, they fire
// unattended on a clock, and two write-jobs can hit the store at once — a slow run
// overrunning into the next trigger, or two different jobs at the same minute. That
// races the store: double-folds, or a keeper superseded between selection and fold.
//
// This is a KV-backed single-flight lock (reusing VERSION_CACHE). A job ACQUIRES the
// shared "cognitive" lease before writing; if another holds it, the job SKIPS (the
// next tick catches the work); on completion it RELEASES. The KV key carries an
// expiration_ttl, so a job that crashes without releasing can't jam the door shut —
// the lease self-expires well before the next nightly trigger.
//
// NB: Cloudflare KV has no compare-and-set, so `acquire` is read-then-write with a
// sub-second TOCTOU window. That's why the cron roster STAGGERS the jobs onto
// different minutes — the lease then only ever arbitrates the overrun case, never a
// simultaneous acquire. CSCC's keeper-recheck in the fold path is the belt-and-
// suspenders for any residual race. For a single-operator store this is the right
// level; a hard mutex would mean a D1 leases table + atomic conditional update,
// which isn't worth the migration here.

use serde::{Deserialize, Serialize};
use worker::{Env, Result};

const KV_BINDING: &str = "VERSION_CACHE";

/// Lease lifetime. Longer than any cognitive-write run (minutes), far shorter than
/// the daily inter-trigger gap — so a crashed run's lease clears before the next night.
const LEASE_TTL_SECS: u64 = 3600;

/// The single shared lease name. CSCC, orient-run, and the dialectic all take THIS
/// one, so no two cognitive-write jobs ever run against the store at once.
pub const COGNITIVE: &str = "cognitive";

#[derive(Serialize, Deserialize)]
struct Lease {
    token: String,
    /// RFC3339 — informational only; KV's own expiration_ttl is the real expiry.
    acquired_at: String,
}

fn key(name: &str) -> String {
    format!("lease:{}", name)
}

/// Try to take the named lease. Returns `Some(token)` if acquired — the caller must
/// `release` it when done — or `None` if a live holder already has it, in which case
/// the caller should skip this run. A lease past its TTL is already gone from KV, so
/// it reads as free.
pub async fn acquire(env: &Env, name: &str) -> Result<Option<String>> {
    let kv = env.kv(KV_BINDING)?;
    let k = key(name);
    if kv.get(k.as_str()).text().await?.is_some() {
        return Ok(None); // held by a live run
    }
    let token = uuid::Uuid::new_v4().to_string();
    let lease = Lease {
        token: token.clone(),
        acquired_at: chrono::Utc::now().to_rfc3339(),
    };
    kv.put(k.as_str(), serde_json::to_string(&lease).unwrap_or_default())?
        .expiration_ttl(LEASE_TTL_SECS)
        .execute()
        .await?;
    Ok(Some(token))
}

/// Release the lease iff we still hold it (token matches) — never delete another
/// holder's lease. A no-op if it has already expired or been taken over.
pub async fn release(env: &Env, name: &str, token: &str) -> Result<()> {
    let kv = env.kv(KV_BINDING)?;
    let k = key(name);
    if let Some(raw) = kv.get(k.as_str()).text().await? {
        if let Ok(l) = serde_json::from_str::<Lease>(&raw) {
            if l.token == token {
                kv.delete(k.as_str()).await?;
            }
        }
    }
    Ok(())
}
