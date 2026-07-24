//! Beacon image generation — the bake's write-side. Calls Ideogram with a
//! memory-derived prompt, downloads the (ephemeral) result, and stores the PNG
//! in R2 under `beacon/samples/`. The read-side (`/beacon/raw`) then decodes +
//! dithers + serves it via the pipeline already proven.
//!
//! Generation is synchronous but slow (seconds–30s), so this is meant to run
//! *off* the device-fetch path — a cron/queue bake. The manual `/beacon/bake`
//! endpoint triggers a single generation for testing.
//!
//! Contract (verified against developer.ideogram.ai, Ideogram 4.0 — v4 renders
//! text in-image, which v3 wouldn't):
//!   POST https://api.ideogram.ai/v1/ideogram-v4/generate
//!   header `Api-Key`, body multipart/form-data, prompt field `text_prompt`,
//!   response `{ data: [{ url, is_image_safe, .. }] }`, url ephemeral.

use worker::*;

const IDEOGRAM_URL: &str = "https://api.ideogram.ai/v1/ideogram-v4/generate";
const BOUNDARY: &str = "----OneiroBeaconBoundary7f3a91c2b8";

/// Generate one image from `prompt`, download it, store it in R2, return the
/// stored key. Errors (rather than stores) if Ideogram flags it unsafe.
pub async fn generate_and_store(env: &Env, prompt: &str) -> Result<String> {
    let api_key = env.secret("IDEOGRAM_API_KEY")?.to_string();

    // Hand-build the multipart body — all fields are simple strings, so this
    // sidesteps any FormData-to-JsValue friction and uses the proven String body.
    let mut form = String::new();
    for (name, value) in [
        ("text_prompt", prompt),
        ("rendering_speed", "DEFAULT"),
        ("resolution", "2880x1440"),
    ] {
        form.push_str(&format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        ));
    }
    form.push_str(&format!("--{BOUNDARY}--\r\n"));

    let headers = Headers::new();
    headers.set("Api-Key", &api_key)?;
    headers.set(
        "content-type",
        &format!("multipart/form-data; boundary={BOUNDARY}"),
    )?;

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(form.into()));
    let mut resp = Fetch::Request(Request::new_with_init(IDEOGRAM_URL, &init)?)
        .send()
        .await?;

    if resp.status_code() >= 400 {
        let err = resp.text().await.unwrap_or_else(|_| "no body".to_string());
        return Err(Error::RustError(format!(
            "Ideogram {}: {}",
            resp.status_code(),
            err
        )));
    }

    let json: serde_json::Value = serde_json::from_str(&resp.text().await?)
        .map_err(|e| Error::RustError(format!("parse Ideogram response: {e}")))?;
    let first = json["data"]
        .as_array()
        .and_then(|a| a.first())
        .ok_or_else(|| Error::RustError("Ideogram response had no data[0]".to_string()))?;

    if first["is_image_safe"].as_bool() == Some(false) {
        return Err(Error::RustError(
            "Ideogram flagged the image unsafe".to_string(),
        ));
    }
    let url = first["url"]
        .as_str()
        .ok_or_else(|| Error::RustError("Ideogram data[0] missing url".to_string()))?;

    // The url is ephemeral — download immediately.
    let mut img = Fetch::Request(Request::new(url, Method::Get)?).send().await?;
    if img.status_code() >= 400 {
        return Err(Error::RustError(format!(
            "image download {}",
            img.status_code()
        )));
    }
    let png = img.bytes().await?;

    let bucket = env.bucket("IMAGES")?;
    let key = format!("beacon/samples/gen-{}.png", uuid::Uuid::new_v4());
    bucket.put(&key, png).execute().await?;

    Ok(key)
}
