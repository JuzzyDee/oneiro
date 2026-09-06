//! Beacon battery telemetry + low-battery email alert.
//!
//! The colour Spectra panel can't do a partial-refresh low-battery warning on
//! the glass, so the alert lives here on the server instead. The device stamps
//! its fuel-gauge state-of-charge into an `X-Beacon-Battery` header on every
//! `/beacon/raw` fetch (twice a day). We keep a *single overwritten value* in KV
//! — the latest percent plus an "already alerted" flag, no history — and email
//! once when it crosses the low line, re-arming only after a real charge.

use serde::{Deserialize, Serialize};
use worker::{Env, Fetch, Headers, Method, Request, RequestInit, Result};

const KV_BINDING: &str = "VERSION_CACHE"; // general-purpose cache KV (shared)
const KV_KEY: &str = "beacon:battery";

const LOW_THRESHOLD_PCT: u8 = 10; // email at or below this
const CLEAR_THRESHOLD_PCT: u8 = 25; // clear the flag (re-arm) at or above this — hysteresis

// The `from` address must be on a domain verified with the email provider
// (halflegless.com, via Resend). `to` is where the alert lands.
const MAIL_FROM: &str = "Beacon <beacon@halflegless.com>";
const MAIL_TO: &str = "juzzydee@gmail.com";

#[derive(Serialize, Deserialize, Default)]
struct BatteryState {
    pct: u8,
    alerted: bool,
}

/// Record the latest reported SoC and email once if it just crossed the low
/// line. Best-effort: the caller logs and swallows any error so battery
/// telemetry can never fail the image serve.
pub async fn record_and_maybe_alert(env: &Env, pct: u8) -> Result<()> {
    let kv = env.kv(KV_BINDING)?;

    let mut state: BatteryState = kv
        .get(KV_KEY)
        .text()
        .await?
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let was_alerted = state.alerted;
    state.pct = pct;

    // Hysteresis: fire once on the way down; re-arm only after a real charge.
    let mut send = false;
    if pct <= LOW_THRESHOLD_PCT && !state.alerted {
        state.alerted = true;
        send = true;
    } else if pct >= CLEAR_THRESHOLD_PCT && state.alerted {
        state.alerted = false;
    }

    // Persist first, with the flag already set, so a mail hiccup can never cause
    // a re-send loop on the next fetch.
    if let Err(e) = kv
        .put(KV_KEY, serde_json::to_string(&state).unwrap_or_default())?
        .execute()
        .await
    {
        worker::console_error!("beacon battery KV write failed: {:?}", e);
    }

    if send {
        worker::console_log!(
            "beacon battery low ({}%, was_alerted={}) — sending email",
            pct,
            was_alerted
        );
        send_low_battery_email(env, pct).await?;
    }
    Ok(())
}

async fn send_low_battery_email(env: &Env, pct: u8) -> Result<()> {
    // Gated on the Resend key: until it's set, log and skip rather than error,
    // so the feature degrades quietly before the provider is wired up.
    let api_key = match env.secret("RESEND_API_KEY") {
        Ok(s) => s.to_string(),
        Err(_) => {
            worker::console_log!("RESEND_API_KEY not set — skipping low-batt email");
            return Ok(());
        }
    };

    let body = serde_json::json!({
        "from": MAIL_FROM,
        "to": [MAIL_TO],
        "subject": "Beacon low battery",
        "text": format!("Beacon low battery — {}%.\n\nCharge to continue receiving images?", pct),
    });

    let headers = Headers::new();
    headers.set("Authorization", &format!("Bearer {}", api_key))?;
    headers.set("content-type", "application/json")?;

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(body.to_string().into()));

    let req = Request::new_with_init("https://api.resend.com/emails", &init)?;
    let mut resp = Fetch::Request(req).send().await?;

    if resp.status_code() >= 400 {
        let err = resp.text().await.unwrap_or_else(|_| "no body".to_string());
        return Err(worker::Error::RustError(format!(
            "Resend {} : {}",
            resp.status_code(),
            err
        )));
    }
    worker::console_log!("beacon low-batt email sent ({}%)", pct);
    Ok(())
}
