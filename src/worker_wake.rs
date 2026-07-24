//! Wake-target evaluator — the watchers that fire the model's standing interests.
//! Cheap, code-only condition checks (no model inference); runs on the scheduled
//! handler, lease-free, because it only touches its own table. When a target's
//! condition is met it's marked fired with the observed detail; the fire surfaces
//! to the model on its next return (recall_wakes), carrying the `why` a past
//! instance set. Event-driven, not a clock fuse — the cron is just the polling
//! cadence; the *firing* tracks a real event the model chose to care about.
//!
//! One check kind to start — `http` (the server-unreachable / content-alert
//! family). The register is kind-agnostic; new kinds slot into `evaluate_one`.

use crate::worker_store::{self, WakeTarget};
use worker::*;

/// HTTP check config — the JSON stored in a target's `check_config`.
#[derive(serde::Deserialize)]
struct HttpCheck {
    url: String,
    /// `unreachable` — fire on fetch error or status >= 400.
    /// `contains`    — fire when `needle` appears in the response body.
    /// `absent`      — fire when `needle` is no longer in the body.
    fire_when: String,
    #[serde(default)]
    needle: Option<String>,
}

fn short(id: &str) -> &str {
    &id[..8.min(id.len())]
}

/// Evaluate every active wake-target; fire those whose condition is met. Lease-
/// free and cheap — touches only `wake_targets`, never the cognitive store.
pub async fn evaluate_wake_targets(env: &Env) -> Result<()> {
    let db = env.d1("DB")?;
    let targets = worker_store::list_active_wake_targets(&db)
        .await
        .map_err(|e| Error::RustError(format!("list wake targets: {e:?}")))?;
    for t in targets {
        let _ = worker_store::touch_wake_checked(&db, &t.id).await;
        match evaluate_one(&t).await {
            Ok(Some(detail)) => {
                console_log!("wake-target {} fired: {}", short(&t.id), detail);
                let _ = worker_store::mark_wake_fired(&db, &t.id, &detail).await;
            }
            Ok(None) => {}
            // A broken watcher shouldn't masquerade as a real event — log, don't fire.
            Err(e) => console_error!("wake-target {} eval error: {}", short(&t.id), e),
        }
    }
    Ok(())
}

/// Evaluate one target. `Ok(Some(detail))` = fired (detail = what was observed);
/// `Ok(None)` = condition not met; `Err` = the check itself failed.
async fn evaluate_one(t: &WakeTarget) -> std::result::Result<Option<String>, String> {
    match t.check_kind.as_str() {
        "http" => {
            let cfg: HttpCheck = serde_json::from_str(&t.check_config)
                .map_err(|e| format!("bad http config: {e}"))?;
            let req = Request::new(&cfg.url, Method::Get)
                .map_err(|e| format!("bad request: {e:?}"))?;
            let resp = Fetch::Request(req).send().await;
            match cfg.fire_when.as_str() {
                "unreachable" => match resp {
                    Err(_) => Ok(Some(format!("{} is unreachable", cfg.url))),
                    Ok(r) if r.status_code() >= 400 => {
                        Ok(Some(format!("{} returned HTTP {}", cfg.url, r.status_code())))
                    }
                    Ok(_) => Ok(None),
                },
                "contains" | "absent" => {
                    let needle = cfg
                        .needle
                        .as_deref()
                        .ok_or_else(|| "contains/absent needs a `needle`".to_string())?;
                    let mut r = resp.map_err(|e| format!("fetch failed: {e:?}"))?;
                    let body = r.text().await.map_err(|e| format!("read body: {e:?}"))?;
                    let present = body.contains(needle);
                    let fire = if cfg.fire_when == "contains" { present } else { !present };
                    Ok(fire.then(|| {
                        let verb = if cfg.fire_when == "contains" {
                            "now contains"
                        } else {
                            "no longer contains"
                        };
                        format!("{} {} \"{}\"", cfg.url, verb, needle)
                    }))
                }
                other => Err(format!("unknown fire_when: {other}")),
            }
        }
        other => Err(format!("unknown check_kind: {other}")),
    }
}
