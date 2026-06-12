//! Async encode via the Anthropic Message Batches API (CLA-132).
//!
//! The sync path (`worker_encode::encode_one`) makes one blocking Haiku call per
//! episodic — fine for `/admin/encode-run`, fatal in the queue consumer for a
//! large compaction dump, which overran the wall-time budget and stranded the
//! episodic with no retry. This path submits the judgment as a batch (50%
//! cheaper, processed off our clock) and dispatches the result from a delayed
//! poll message, so no worker ever holds the inference.
//!
//! Flow (pure-queue, event-driven — see lib.rs `queue`):
//!   capture → write episodic → `submit_batch` → enqueue delayed PollBatch
//!   PollBatch → `poll_batch` → ended? dispatch + done : re-enqueue delayed poll

use crate::memory::Memory;
use crate::{worker_encode, worker_store};
use serde_json::{json, Value};
use worker::{D1Database, Env, Fetch, Headers, Method, Request, RequestInit, Result};

const BATCHES_URL: &str = "https://api.anthropic.com/v1/messages/batches";

/// Auth headers, mirroring `worker_encode::encode_via_claude`: an `sk-ant-api…`
/// key authenticates via `x-api-key`, any other token (the OAuth `sk-ant-oat…`)
/// via `Authorization: Bearer`. (Small duplication of one stable block.)
fn anthropic_headers(token: &str) -> Result<Headers> {
    let headers = Headers::new();
    if token.starts_with("sk-ant-api") {
        headers.set("x-api-key", token)?;
    } else {
        headers.set("Authorization", &format!("Bearer {}", token))?;
    }
    headers.set("anthropic-version", "2023-06-01")?;
    headers.set("content-type", "application/json")?;
    Ok(headers)
}

/// Authenticated GET against the Anthropic API. Returns (status, body).
async fn anthropic_get(env: &Env, url: &str) -> Result<(u16, String)> {
    let token = env.secret("CLAUDE_CODE_OAUTH_TOKEN")?.to_string();
    let headers = anthropic_headers(&token)?;
    let mut init = RequestInit::new();
    init.with_method(Method::Get).with_headers(headers);
    let req = Request::new_with_init(url, &init)?;
    let mut resp = Fetch::Request(req).send().await?;
    let status = resp.status_code();
    let body = resp.text().await.unwrap_or_default();
    Ok((status, body))
}

/// Submit a set of episodics as ONE Anthropic batch (custom_id = episodic_id),
/// record it in the ledger, and return the batch id. `None` if `episodics` is
/// empty. Submit returns immediately (the batch processes off our clock), so the
/// caller never blocks — it just enqueues a delayed poll for the returned id.
pub async fn submit_batch(
    env: &Env,
    db: &D1Database,
    episodics: &[Memory],
) -> Result<Option<String>> {
    if episodics.is_empty() {
        return Ok(None);
    }

    let mut requests = Vec::with_capacity(episodics.len());
    for ep in episodics {
        let params = worker_encode::build_request_for(env, db, ep).await;
        requests.push(json!({ "custom_id": ep.id, "params": params }));
    }
    let body = json!({ "requests": requests });

    let token = env.secret("CLAUDE_CODE_OAUTH_TOKEN")?.to_string();
    let headers = anthropic_headers(&token)?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(body.to_string().into()));
    let req = Request::new_with_init(BATCHES_URL, &init)?;
    let mut resp = Fetch::Request(req).send().await?;
    if resp.status_code() >= 400 {
        let err = resp.text().await.unwrap_or_default();
        return Err(worker::Error::RustError(format!(
            "batch submit {}: {}",
            resp.status_code(),
            err
        )));
    }

    let parsed: Value = serde_json::from_str(&resp.text().await?)
        .map_err(|e| worker::Error::RustError(format!("parse batch submit: {}", e)))?;
    let batch_id = parsed
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| worker::Error::RustError("batch submit: no id in response".to_string()))?
        .to_string();

    let ep_ids: Vec<String> = episodics.iter().map(|e| e.id.clone()).collect();
    worker_store::insert_encode_batch(db, &batch_id, &ep_ids).await?;
    worker::console_log!(
        "encode-batch submitted {} ({} episodic(s))",
        &batch_id,
        episodics.len()
    );
    Ok(Some(batch_id))
}

/// Outcome of polling one batch.
pub enum PollOutcome {
    /// Results fetched and dispatched (or failures recorded) — the batch is done.
    Done,
    /// Anthropic hasn't finished — the caller should re-enqueue a delayed poll.
    StillWaiting,
}

/// Poll one in-flight batch. If it has `ended`, fetch the results, dispatch each
/// (or record a failure), and mark the batch dispatched. If still processing,
/// bump the poll counter and signal `StillWaiting`. A batch that comes back in a
/// terminal non-`ended` state is treated as ended (its per-result errors are
/// recorded by `dispatch_results`).
pub async fn poll_batch(
    env: &Env,
    db: &D1Database,
    batch: &worker_store::EncodeBatchRow,
) -> Result<PollOutcome> {
    let url = format!("{}/{}", BATCHES_URL, batch.batch_id);
    let (status, body) = anthropic_get(env, &url).await?;
    if status >= 400 {
        return Err(worker::Error::RustError(format!(
            "batch status {}: {}",
            status, body
        )));
    }
    let parsed: Value = serde_json::from_str(&body)
        .map_err(|e| worker::Error::RustError(format!("parse batch status: {}", e)))?;
    let processing = parsed
        .get("processing_status")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if processing != "ended" {
        worker_store::bump_encode_batch_poll(db, &batch.batch_id).await?;
        return Ok(PollOutcome::StillWaiting);
    }

    // Ended — fetch results (.jsonl, one line per request) and dispatch them.
    let (rstatus, rbody) = anthropic_get(env, &format!("{}/results", url)).await?;
    if rstatus >= 400 {
        return Err(worker::Error::RustError(format!(
            "batch results {}: {}",
            rstatus, rbody
        )));
    }
    dispatch_results(env, db, &batch.batch_id, &rbody).await;
    worker_store::set_encode_batch_status(db, &batch.batch_id, "dispatched").await?;
    Ok(PollOutcome::Done)
}

/// Parse the results JSONL and dispatch each succeeded result to its episodic;
/// record a failure row for each errored one. Best-effort per line — a single
/// bad result never aborts the rest of the batch.
async fn dispatch_results(env: &Env, db: &D1Database, batch_id: &str, jsonl: &str) {
    for line in jsonl.lines().filter(|l| !l.trim().is_empty()) {
        let row: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                worker::console_log!("batch result parse skip: {}", e);
                continue;
            }
        };
        let custom_id = row.get("custom_id").and_then(|v| v.as_str()).unwrap_or("");
        let result = row.get("result");
        let result_type = result
            .and_then(|r| r.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or("");

        if result_type != "succeeded" {
            let _ = worker_store::record_encode_failure(db, custom_id, batch_id, "result_errored").await;
            worker::console_log!("encode-batch result errored for {}", custom_id);
            continue;
        }

        let message = match result.and_then(|r| r.get("message")) {
            Some(m) => m,
            None => {
                let _ = worker_store::record_encode_failure(db, custom_id, batch_id, "result_errored").await;
                continue;
            }
        };
        let decisions = match worker_encode::parse_encode_decisions(message) {
            Ok(d) => d,
            Err(e) => {
                worker::console_log!("parse decisions for {}: {:?}", custom_id, e);
                let _ = worker_store::record_encode_failure(db, custom_id, batch_id, "result_errored").await;
                continue;
            }
        };

        // Load the episodic and apply the decisions through the shared write side.
        match worker_store::get_many(db, &[custom_id]).await {
            Ok(found) => match found.into_iter().next() {
                Some(ep) => {
                    // Autopsy this batch result (the first-submission path) before
                    // dispatch — candidate_count is -1 here (found at submit, not
                    // threaded through); the rest is the model's own envelope.
                    worker_encode::capture_diagnostic(
                        db,
                        custom_id,
                        "queue-batch",
                        -1,
                        ep.content.chars().count() as i64,
                        200,
                        message,
                        decisions.len() as i64,
                    )
                    .await;
                    let _ = worker_encode::dispatch_decisions(env, db, &ep, &decisions).await;
                }
                None => worker::console_log!("encode-batch: episodic {} gone at dispatch", custom_id),
            },
            Err(e) => worker::console_log!("encode-batch: load {} failed: {:?}", custom_id, e),
        }
    }
}
