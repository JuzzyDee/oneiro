// worker_mcp.rs — Streamable HTTP MCP endpoint for the wasm32 worker.
//
// rmcp's bundled streamable-HTTP transport doesn't compile to wasm32
// (axum/tower/hyper internals), so we hand-roll the JSON-RPC layer
// ourselves. Cheaper than wrestling rmcp into a transport-less mode.
//
// Phase 6a (this commit) implements:
//   - initialize           — handshake
//   - tools/list           — exposes the rover-relevant tool surface
//   - tools/call recall    — full semantic recall (embed → Vectorize → D1)
//   - tools/call remember  — full write (D1 INSERT + Vectorize upsert)
//
// Subsequent phases add the remaining tools (recall_check,
// recall_specific, review, reframe, forget, reflect, remember_with_image)
// and the OAuth path for non-rover callers.

use crate::memory::{Memory, MemoryType};
use crate::worker_auth_ctx::{self, AuthCtx};
use crate::worker_orient::format_memory;
use crate::{worker_embed, worker_store, worker_vectorize};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use worker::{D1Database, Env, Response, Result};

/// MCP protocol version we negotiate to during `initialize`.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// JSON-RPC 2.0 request envelope. Params/id are typed loosely because
/// MCP uses both numeric and string ids and notifications (no id at all).
#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    method: String,
    #[serde(default)]
    params: Value,
    #[serde(default)]
    id: Option<Value>,
}

/// Build a JSON-RPC success response.
fn rpc_ok(id: Option<Value>, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

/// Build a JSON-RPC error response.
fn rpc_err(id: Option<Value>, code: i32, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() },
    })
}

// JSON-RPC standard error codes used by MCP:
const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;

/// Entry point — called by lib.rs after auth validation. Sets AUTH_CTX
/// scope so check_scope() inside tool handlers sees the resolved caller,
/// parses the JSON-RPC body, dispatches.
pub async fn handle(env: &Env, body: &str, auth: AuthCtx) -> Result<Response> {
    let req: JsonRpcRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => {
            return Response::from_json(&rpc_err(
                None,
                PARSE_ERROR,
                format!("Parse error: {}", e),
            ));
        }
    };

    let id = req.id.clone();
    let env_owned = env.clone();
    let response = worker_auth_ctx::AUTH_CTX
        .scope(auth, async move {
            dispatch(&env_owned, &req).await
        })
        .await;

    match response {
        Ok(value) => Response::from_json(&rpc_ok(id, value)),
        Err(rpc_error) => Response::from_json(&rpc_error),
    }
}

/// Returns the `result` value on success, a full JSON-RPC error envelope
/// on failure (with `id` already filled).
async fn dispatch(env: &Env, req: &JsonRpcRequest) -> std::result::Result<Value, Value> {
    let id = req.id.clone();
    match req.method.as_str() {
        "initialize" => Ok(handle_initialize()),
        "notifications/initialized" => Ok(Value::Null),
        "tools/list" => Ok(handle_tools_list()),
        "tools/call" => handle_tools_call(env, req).await.map_err(|e| {
            rpc_err(id, INTERNAL_ERROR, format!("Tool dispatch failed: {}", e))
        }),
        other => Err(rpc_err(
            id,
            METHOD_NOT_FOUND,
            format!("Method `{}` not implemented", other),
        )),
    }
}

fn handle_initialize() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": "oneiro",
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

fn handle_tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "recall_orient",
                "description": "Conversation-start orientation. Returns all orientation \
                                memories (always-loaded identity context, pinned at strength \
                                1.0) plus the N most recently created episodic memories \
                                (chronological-recent, default N=3). No semantic search, no \
                                embedding, no guessing at conversation start. Call this \
                                first — the orientation is who you are with this user and \
                                the recents are what's been happening lately. If a specific \
                                topic later needs surfacing, use recall_check.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "n": {
                            "type": "integer",
                            "description": "Number of most-recent episodic memories to \
                                            include (default 3, max 50). Pass 0 for \
                                            orientation only. Values above the max are \
                                            clamped server-side."
                        }
                    }
                }
            },
            {
                "name": "remember",
                "description": "Store a new memory. Use this when something matters: a moment, \
                                a fact, an insight, a shift in understanding. You decide what's \
                                worth keeping. Not everything is.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "The memory content — what happened, what was \
                                            learned, what matters."
                        },
                        "summary": {
                            "type": "string",
                            "description": "A one-line summary for quick scanning during recall."
                        },
                        "memory_type": {
                            "type": "string",
                            "enum": ["episodic", "semantic", "orientation"],
                            "description": "episodic (events), semantic (knowledge), or \
                                            orientation (identity)."
                        },
                        "entity": {
                            "type": "string",
                            "description": "Which person or entity this memory relates to \
                                            (e.g. 'justin', 'chopper')."
                        },
                        "tags": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Tags for association."
                        }
                    },
                    "required": ["content", "summary", "memory_type"]
                }
            },
            {
                "name": "remember_with_image",
                "description": "Store a memory with an attached image. The image bytes (base64) \
                                are content-addressed into R2 — duplicate uploads of the same \
                                image are deduplicated automatically. Used primarily by the \
                                rover heartbeat to record observations with the current camera \
                                frame.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "content": { "type": "string" },
                        "summary": { "type": "string" },
                        "memory_type": {
                            "type": "string",
                            "enum": ["episodic", "semantic", "orientation"]
                        },
                        "entity": { "type": "string" },
                        "tags": { "type": "array", "items": { "type": "string" } },
                        "image_base64": {
                            "type": "string",
                            "description": "Base64-encoded image bytes (no data: URI prefix)."
                        },
                        "image_mime": {
                            "type": "string",
                            "description": "MIME type — image/jpeg, image/png, or image/webp."
                        }
                    },
                    "required": ["content", "summary", "memory_type", "image_base64", "image_mime"]
                }
            },
            {
                "name": "recall_image",
                "description": "Retrieve an image attached to a memory. Takes the memory's id \
                                (full UUID or the 8-char prefix shown in recall output). Returns \
                                MCP content with the memory's summary text and the image bytes.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "memory_id": {
                            "type": "string",
                            "description": "Full UUID or 8-char prefix from a prior recall."
                        }
                    },
                    "required": ["memory_id"]
                }
            },
            {
                "name": "recall_check",
                "description": "Lightweight mid-conversation memory lookup. Stricter similarity \
                                threshold than recall, no orientation prepended. Use when the \
                                conversation shifts topic and you want a quick `do I know \
                                anything about this' check without a full recall reload. \
                                Optional metadata filters narrow the search to memories about a \
                                specific entity, of a specific type, or carrying specific tags — \
                                filters compose (e.g. entity=chopper + memory_type=semantic) \
                                and apply independently of the semantic similarity ranking.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "topic": { "type": "string" },
                        "min_similarity": {
                            "type": "number",
                            "description": "0.0-1.0, default 0.6. Higher = more selective."
                        },
                        "limit": { "type": "integer", "description": "Default 5." },
                        "entity": {
                            "type": "string",
                            "description": "Filter to memories whose entity matches this value \
                                            exactly (e.g. 'chopper', 'rover', 'justin'). \
                                            Case-sensitive."
                        },
                        "memory_type": {
                            "type": "string",
                            "enum": ["episodic", "semantic", "orientation"],
                            "description": "Filter to memories of this type."
                        },
                        "tags": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Filter to memories whose tag list includes at least \
                                            one of these tags (any-of, not all-of)."
                        }
                    },
                    "required": ["topic"]
                }
            },
            {
                "name": "recall_specific",
                "description": "Retrieve specific memories by id list — the deliberate choice \
                                to think about something. Returns full content (not summaries). \
                                Strongest co-activation signal (the conscious choice to surface \
                                these together shapes future recall).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "memory_ids": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Full UUIDs or 8-char prefixes."
                        }
                    },
                    "required": ["memory_ids"]
                }
            },
            {
                "name": "reframe",
                "description": "Update an existing memory's content + summary. Use when your \
                                understanding of a memory has evolved — reframing is not the \
                                same as forgetting. Re-embeds and updates Vectorize so future \
                                semantic recall uses the new framing.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "memory_id": { "type": "string" },
                        "new_content": { "type": "string" },
                        "new_summary": { "type": "string" }
                    },
                    "required": ["memory_id", "new_content", "new_summary"]
                }
            },
            {
                "name": "forget",
                "description": "Forget a memory — DELETE plus a tombstone. Use sparingly; this \
                                is consolidation pruning, not casual deletion. Orientation \
                                memories CANNOT be forgotten — identity is non-negotiable. \
                                Returns whether a row was actually removed.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "memory_id": { "type": "string" },
                        "reason": {
                            "type": "string",
                            "description": "Why this memory is no longer needed."
                        }
                    },
                    "required": ["memory_id"]
                }
            },
            {
                "name": "reflect",
                "description": "Consolidation at natural breakpoints. Writes a reflection \
                                episodic memory capturing the conversation's highlights, and \
                                optionally batch-updates a set of memories whose framing has \
                                evolved in the same session.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "conversation_highlights": {
                            "type": "string",
                            "description": "What mattered in this conversation, written first-\
                                            person and prose-like."
                        },
                        "memories_to_update": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "memory_id": { "type": "string" },
                                    "new_content": { "type": "string" },
                                    "new_summary": { "type": "string" }
                                },
                                "required": ["memory_id", "new_content", "new_summary"]
                            }
                        }
                    },
                    "required": ["conversation_highlights"]
                }
            }
        ]
    })
}

async fn handle_tools_call(
    env: &Env,
    req: &JsonRpcRequest,
) -> std::result::Result<Value, String> {
    #[derive(Deserialize)]
    struct ToolCall {
        name: String,
        #[serde(default)]
        arguments: Value,
    }
    let call: ToolCall = serde_json::from_value(req.params.clone())
        .map_err(|e| format!("invalid tools/call params: {}", e))?;

    let db = env.d1("DB").map_err(|e| format!("db binding: {:?}", e))?;

    // Three-gate check (scope/rate/audit) before invoking the handler.
    worker_auth_ctx::check_scope(&db, &call.name).await?;

    // Two of the four tools return a single text content; the others return
    // multi-part content (text + image). Branch on tool name accordingly.
    match call.name.as_str() {
        "recall_orient" => {
            let text = tool_recall_orient(env, &db, call.arguments).await?;
            Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            }))
        }
        "remember" => {
            let text = tool_remember(env, &db, call.arguments).await?;
            Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            }))
        }
        "remember_with_image" => {
            let text = tool_remember_with_image(env, &db, call.arguments).await?;
            Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            }))
        }
        "recall_image" => {
            let content = tool_recall_image(env, &db, call.arguments).await?;
            Ok(json!({
                "content": content,
                "isError": false,
            }))
        }
        "recall_check" => {
            let text = tool_recall_check(env, &db, call.arguments).await?;
            Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            }))
        }
        "recall_specific" => {
            let text = tool_recall_specific(&db, call.arguments).await?;
            Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            }))
        }
        "reframe" => {
            let text = tool_reframe(env, &db, call.arguments).await?;
            Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            }))
        }
        "forget" => {
            let text = tool_forget(env, &db, call.arguments).await?;
            Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            }))
        }
        "reflect" => {
            let text = tool_reflect(env, &db, call.arguments).await?;
            Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            }))
        }
        other => Err(format!("unknown tool: {}", other)),
    }
}

#[derive(Deserialize)]
struct RecallOrientArgs {
    #[serde(default)]
    n: Option<usize>,
}

/// Maximum number of recent episodics `recall_orient` will return,
/// regardless of caller request. Bounds payload size + D1 read cost —
/// the entry-point tool should be fast and small. Anything beyond this
/// is a "browse the store" job and belongs in `review`-style tooling.
const MAX_RECALL_ORIENT_N: usize = 50;

/// recall_orient — the conversation-start tool (CLA-103). Returns all
/// orientation memories plus N most recent episodics (default N=3,
/// clamped to MAX_RECALL_ORIENT_N). No embed, no Vectorize, no MMR.
/// The work that makes this payload good already happened upstream
/// during reflect/dialectic passes; here we just surface it.
/// Conscious-call semantics: touches surfaced memories (Hebbian
/// reinforcement) and co-activates the recents (they were surfaced
/// together).
async fn tool_recall_orient(
    env: &Env,
    db: &D1Database,
    args: Value,
) -> std::result::Result<String, String> {
    let args: RecallOrientArgs = serde_json::from_value(args)
        .map_err(|e| format!("invalid recall_orient args: {}", e))?;
    let n = args.n.unwrap_or(3).min(MAX_RECALL_ORIENT_N);

    let orientation = worker_store::get_orientation(db)
        .await
        .map_err(|e| format!("get_orientation: {:?}", e))?;
    let recent = if n == 0 {
        Vec::new()
    } else {
        worker_store::get_recent_episodics(db, n)
            .await
            .map_err(|e| format!("get_recent_episodics: {:?}", e))?
    };
    let counts = worker_store::count_by_type(db).await.unwrap_or((0, 0, 0));

    // Touch — Hebbian reinforcement for memories surfaced in a
    // conscious call. Orientation is no-op for strength (pinned at 1.0)
    // but still counts the access. Recents get a decay reset, which
    // matches the "still salient enough to be the first thing the next
    // instance sees" signal.
    for m in orientation.iter().chain(recent.iter()) {
        let _ = worker_store::touch(db, &m.id).await;
    }
    // Co-activate the recents — they were surfaced together. Skip
    // orientation in the coact set, matching the existing recall path
    // (orientation is always-loaded, so co-activation with it is noise).
    let coact_ids: Vec<&str> = recent.iter().map(|m| m.id.as_str()).collect();
    if coact_ids.len() >= 2 {
        let _ = worker_store::record_co_activation(db, &coact_ids).await;
    }

    let mut out = crate::worker_orient::format_payload(&orientation, Some(&recent), counts);

    // Update-prompt tail — same shape as the legacy recall path (CLA-102).
    // Fail-soft: if the check errors, we just don't append. The /orientation
    // hook endpoint (CLA-105) deliberately omits this because it fires
    // automatically and would surface the prompt before the model can act
    // on it; the conscious recall_orient call is the right surface.
    if let Ok(Some(update)) = crate::worker_version::check_for_update(env).await {
        out.push_str("\n── Oneiro update available ──\n");
        out.push_str(&format!(
            "Running {} → latest {}",
            update.current, update.latest
        ));
        if let Some(url) = &update.url {
            out.push_str(&format!("\nRelease notes: {}", url));
        }
        out.push('\n');
    }

    Ok(out)
}

#[derive(Deserialize)]
struct RememberArgs {
    content: String,
    summary: String,
    memory_type: String,
    #[serde(default)]
    entity: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

async fn tool_remember(
    env: &Env,
    db: &D1Database,
    args: Value,
) -> std::result::Result<String, String> {
    let args: RememberArgs =
        serde_json::from_value(args).map_err(|e| format!("invalid remember args: {}", e))?;
    let memory_type = MemoryType::from_str(&args.memory_type)
        .ok_or_else(|| format!("unknown memory_type: {}", args.memory_type))?;

    // recorded_by comes from auth context (server-controlled) per CLA-86.
    let recorded_by = worker_auth_ctx::current_recorded_by();

    let memory = worker_store::create_memory_with_provenance(
        db,
        memory_type,
        args.content.clone(),
        args.summary,
        args.entity,
        args.tags,
        recorded_by,
    )
    .await
    .map_err(|e| format!("create_memory: {:?}", e))?;

    // Embed + upsert to Vectorize so the memory is semantically searchable.
    let embedding = worker_embed::embed_document(env, &args.content)
        .await
        .map_err(|e| format!("embed_document: {:?}", e))?;
    worker_vectorize::upsert_one(env, &memory.id, &embedding)
        .await
        .map_err(|e| format!("vectorize upsert: {:?}", e))?;

    Ok(format!(
        "✓ Remembered: {} (id: {})",
        memory.summary,
        &memory.id[..8]
    ))
}

#[derive(Deserialize)]
struct RememberWithImageArgs {
    content: String,
    summary: String,
    memory_type: String,
    #[serde(default)]
    entity: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    image_base64: String,
    image_mime: String,
}

async fn tool_remember_with_image(
    env: &Env,
    db: &D1Database,
    args: Value,
) -> std::result::Result<String, String> {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

    let args: RememberWithImageArgs = serde_json::from_value(args)
        .map_err(|e| format!("invalid remember_with_image args: {}", e))?;
    let memory_type = MemoryType::from_str(&args.memory_type)
        .ok_or_else(|| format!("unknown memory_type: {}", args.memory_type))?;

    let bytes = BASE64
        .decode(args.image_base64.as_bytes())
        .map_err(|e| format!("invalid base64 image: {}", e))?;

    let bucket = env
        .bucket("IMAGES")
        .map_err(|e| format!("IMAGES bucket binding: {:?}", e))?;

    let recorded_by = worker_auth_ctx::current_recorded_by();

    let memory = worker_store::create_memory_with_image_and_provenance(
        db,
        &bucket,
        memory_type,
        args.content.clone(),
        args.summary,
        args.entity,
        args.tags,
        bytes,
        args.image_mime.clone(),
        recorded_by,
    )
    .await
    .map_err(|e| format!("create_memory_with_image: {:?}", e))?;

    // Embed + upsert. The embedding describes the content, not the image —
    // visual similarity is a future concern (would want CLIP-style embeds).
    let embedding = worker_embed::embed_document(env, &args.content)
        .await
        .map_err(|e| format!("embed_document: {:?}", e))?;
    worker_vectorize::upsert_one(env, &memory.id, &embedding)
        .await
        .map_err(|e| format!("vectorize upsert: {:?}", e))?;

    Ok(format!(
        "✓ Remembered with image: {} (id: {}, mime: {})",
        memory.summary,
        &memory.id[..8],
        memory.image_mime.as_deref().unwrap_or("?")
    ))
}

#[derive(Deserialize)]
struct RecallImageArgs {
    memory_id: String,
}

async fn tool_recall_image(
    env: &Env,
    db: &D1Database,
    args: Value,
) -> std::result::Result<Value, String> {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

    let args: RecallImageArgs =
        serde_json::from_value(args).map_err(|e| format!("invalid recall_image args: {}", e))?;

    let memory = worker_store::find_by_prefix(db, &args.memory_id)
        .await
        .map_err(|e| format!("find_by_prefix: {:?}", e))?;
    let Some(memory) = memory else {
        return Err(format!("No memory found for id `{}`", args.memory_id));
    };
    let Some(hash) = memory.image_hash.as_deref() else {
        return Err(format!(
            "Memory {} has no image attached.",
            &memory.id[..8]
        ));
    };
    let mime = memory.image_mime.as_deref().unwrap_or("image/jpeg");

    let bucket = env
        .bucket("IMAGES")
        .map_err(|e| format!("IMAGES bucket binding: {:?}", e))?;
    let bytes = worker_store::read_image_from_r2(&bucket, hash, mime)
        .await
        .map_err(|e| format!("read_image: {:?}", e))?
        .ok_or_else(|| {
            format!(
                "Image bytes missing in R2 for hash {} (memory row points to a key that isn't there)",
                hash
            )
        })?;

    // Touch — recall_image counts as recall reinforcement.
    let _ = worker_store::touch(db, &memory.id).await;

    let encoded = BASE64.encode(&bytes);
    Ok(json!([
        {
            "type": "text",
            "text": format!(
                "Image for memory {}: {}\n\n{}",
                &memory.id[..8],
                memory.summary,
                memory.content
            )
        },
        {
            "type": "image",
            "data": encoded,
            "mimeType": mime,
        }
    ]))
}

#[derive(Deserialize)]
struct RecallCheckArgs {
    topic: String,
    #[serde(default)]
    min_similarity: Option<f64>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    entity: Option<String>,
    #[serde(default)]
    memory_type: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

async fn tool_recall_check(
    env: &Env,
    db: &D1Database,
    args: Value,
) -> std::result::Result<String, String> {
    let args: RecallCheckArgs =
        serde_json::from_value(args).map_err(|e| format!("invalid recall_check args: {}", e))?;
    let min_similarity = args.min_similarity.unwrap_or(0.6);
    let limit = args.limit.unwrap_or(5);

    // Validate memory_type up front — caller gets a clean error instead of
    // an empty result set caused by a string that doesn't match anything.
    let memory_type_filter = match args.memory_type.as_deref() {
        None => None,
        Some(s) => Some(MemoryType::from_str(s).ok_or_else(|| {
            format!(
                "invalid memory_type: {} (expected episodic, semantic, or orientation)",
                s
            )
        })?),
    };
    let entity_filter = args.entity.as_deref();
    let filters_active =
        entity_filter.is_some() || memory_type_filter.is_some() || !args.tags.is_empty();

    let query_emb = worker_embed::embed_query(env, &args.topic)
        .await
        .map_err(|e| format!("embed_query: {:?}", e))?;
    // Oversample wider when filters are active — entity / memory_type / tags
    // are evaluated post-Vectorize (current index has no metadata; pushdown
    // would require re-upserting every vector with metadata, deferred to
    // CLA-109). A 90%-discriminating filter on a 10-deep oversample leaves
    // ~1 candidate — not enough to fill `limit` or give MMR diversity room.
    // Doubling the floor + ceiling keeps the post-filter survivor pool deep
    // enough without taxing free-tier Vectorize quotas.
    let oversample_base = if filters_active { limit * 8 } else { limit * 4 };
    let oversample_min = if filters_active { 20 } else { 10 };
    let oversample = (oversample_base.clamp(oversample_min, 100)) as u32;
    let matches = worker_vectorize::query_top_k_with_vectors(env, &query_emb, oversample)
        .await
        .map_err(|e| format!("vectorize query: {:?}", e))?;
    let above_threshold: Vec<worker_vectorize::VectorMatchWithVector> = matches
        .into_iter()
        .filter(|m| m.score >= min_similarity)
        .collect();

    if above_threshold.is_empty() {
        return Ok(format!(
            "No memories found for topic: \"{}\" (threshold: {:.2})",
            args.topic, min_similarity
        ));
    }

    // When filters are active, fetch all above-threshold candidates from D1
    // up front so the metadata filter can run before MMR (otherwise MMR
    // would diversify candidates we're about to throw away). When no
    // filters are set, keep the original lean flow: MMR first, then a
    // smaller D1 fetch for just the survivors.
    let (reranked_ids, memories): (Vec<String>, Vec<Memory>) = if filters_active {
        let candidate_ids: Vec<&str> = above_threshold.iter().map(|m| m.id.as_str()).collect();
        let candidates = worker_store::get_many(db, &candidate_ids)
            .await
            .map_err(|e| format!("get_many: {:?}", e))?;
        let candidate_lookup: std::collections::HashMap<&str, &Memory> =
            candidates.iter().map(|m| (m.id.as_str(), m)).collect();
        let filtered_matches: Vec<worker_vectorize::VectorMatchWithVector> = above_threshold
            .iter()
            .filter(|vm| {
                candidate_lookup
                    .get(vm.id.as_str())
                    .is_some_and(|m| m.matches_filter(entity_filter, memory_type_filter, &args.tags))
            })
            .cloned()
            .collect();
        if filtered_matches.is_empty() {
            return Ok(format!(
                "No memories matched filters for topic: \"{}\" (threshold: {:.2})",
                args.topic, min_similarity
            ));
        }
        let reranked = crate::worker_mmr::mmr_rerank(&query_emb, &filtered_matches, limit, 0.7);
        let kept: Vec<Memory> = reranked
            .iter()
            .filter_map(|id| candidates.iter().find(|m| &m.id == id).cloned())
            .collect();
        (reranked, kept)
    } else {
        let reranked = crate::worker_mmr::mmr_rerank(&query_emb, &above_threshold, limit, 0.7);
        let ids: Vec<&str> = reranked.iter().map(String::as_str).collect();
        let mems = worker_store::get_many(db, &ids)
            .await
            .map_err(|e| format!("get_many: {:?}", e))?;
        (reranked, mems)
    };

    // Touch + co-activate — recall_check is still reinforcement.
    let ids: Vec<&str> = reranked_ids.iter().map(String::as_str).collect();
    for m in &memories {
        let _ = worker_store::touch(db, &m.id).await;
    }
    let _ = worker_store::record_co_activation(db, &ids).await;

    let (ep, sem, ori) = worker_store::count_by_type(db).await.unwrap_or((0, 0, 0));
    let mut header = format!(
        "═══ Oneiro Check ═══\nStore: {} ep, {} sem, {} ori | Topic: \"{}\" | Threshold: {:.2}",
        ep, sem, ori, args.topic, min_similarity
    );
    if filters_active {
        let mut bits: Vec<String> = Vec::new();
        if let Some(e) = entity_filter {
            bits.push(format!("entity={}", e));
        }
        if let Some(t) = memory_type_filter {
            bits.push(format!("type={}", t.as_str()));
        }
        if !args.tags.is_empty() {
            bits.push(format!("tags=[{}]", args.tags.join(",")));
        }
        header.push_str(&format!(" | Filters: {}", bits.join(", ")));
    }
    header.push_str("\n\n");
    let mut out = header;

    // Display in MMR selection order, keeping each memory's raw similarity
    // score for the reader (the order is MMR, the per-row sim is cosine).
    let id_to_score: std::collections::HashMap<&str, f64> = above_threshold
        .iter()
        .map(|m| (m.id.as_str(), m.score))
        .collect();
    for id in &reranked_ids {
        if let Some(m) = memories.iter().find(|m| &m.id == id) {
            let sim = id_to_score.get(m.id.as_str()).copied().unwrap_or(0.0);
            out.push_str(&format!(
                "[sim:{:.2} | str:{:.2} | {}]\n{}\n",
                sim,
                m.strength,
                &m.id[..8],
                m.summary
            ));
        }
    }
    Ok(out)
}

#[derive(Deserialize)]
struct RecallSpecificArgs {
    memory_ids: Vec<String>,
}

async fn tool_recall_specific(
    db: &D1Database,
    args: Value,
) -> std::result::Result<String, String> {
    let args: RecallSpecificArgs = serde_json::from_value(args)
        .map_err(|e| format!("invalid recall_specific args: {}", e))?;

    let mut memories: Vec<Memory> = Vec::new();
    for prefix in &args.memory_ids {
        if let Some(m) = worker_store::find_by_prefix(db, prefix)
            .await
            .map_err(|e| format!("find_by_prefix: {:?}", e))?
        {
            memories.push(m);
        }
    }
    if memories.is_empty() {
        return Ok("No memories found for the provided ids.".to_string());
    }

    // Strongest co-activation signal: chosen-together is the bond we
    // want to reinforce.
    let ids: Vec<&str> = memories.iter().map(|m| m.id.as_str()).collect();
    if ids.len() >= 2 {
        let _ = worker_store::record_co_activation(db, &ids).await;
    }
    for m in &memories {
        let _ = worker_store::touch(db, &m.id).await;
    }

    let mut out = String::from("═══ Specific recall ═══\n\n");
    for m in &memories {
        out.push_str(&format_memory(m));
        out.push('\n');
    }
    Ok(out)
}

#[derive(Deserialize)]
struct ReframeArgs {
    memory_id: String,
    new_content: String,
    new_summary: String,
}

async fn tool_reframe(
    env: &Env,
    db: &D1Database,
    args: Value,
) -> std::result::Result<String, String> {
    let args: ReframeArgs =
        serde_json::from_value(args).map_err(|e| format!("invalid reframe args: {}", e))?;

    let memory = worker_store::find_by_prefix(db, &args.memory_id)
        .await
        .map_err(|e| format!("find_by_prefix: {:?}", e))?
        .ok_or_else(|| format!("No memory found for id `{}`", args.memory_id))?;

    let updated = worker_store::reframe(db, &memory.id, &args.new_content, &args.new_summary)
        .await
        .map_err(|e| format!("reframe: {:?}", e))?;
    if !updated {
        return Err(format!("No memory updated for id `{}`", args.memory_id));
    }

    // Re-embed + Vectorize upsert so semantic recall reflects the new framing.
    let embedding = worker_embed::embed_document(env, &args.new_content)
        .await
        .map_err(|e| format!("embed_document: {:?}", e))?;
    worker_vectorize::upsert_one(env, &memory.id, &embedding)
        .await
        .map_err(|e| format!("vectorize upsert: {:?}", e))?;

    Ok(format!(
        "✓ Reframed: {} (id: {})",
        args.new_summary,
        &memory.id[..8]
    ))
}

#[derive(Deserialize)]
struct ForgetArgs {
    memory_id: String,
    #[serde(default)]
    #[allow(dead_code)]
    reason: Option<String>,
}

async fn tool_forget(
    env: &Env,
    db: &D1Database,
    args: Value,
) -> std::result::Result<String, String> {
    let args: ForgetArgs =
        serde_json::from_value(args).map_err(|e| format!("invalid forget args: {}", e))?;

    let memory = worker_store::find_by_prefix(db, &args.memory_id)
        .await
        .map_err(|e| format!("find_by_prefix: {:?}", e))?
        .ok_or_else(|| format!("No memory found for id `{}`", args.memory_id))?;

    if memory.memory_type == MemoryType::Orientation {
        return Err("Orientation memories cannot be forgotten — identity is non-negotiable."
            .to_string());
    }

    let removed = worker_store::forget(db, &memory.id)
        .await
        .map_err(|e| format!("forget: {:?}", e))?;
    if !removed {
        return Ok(format!("No memory removed for id `{}`", args.memory_id));
    }

    // Keep Vectorize in sync — stale vectors that don't resolve to D1
    // rows would haunt future recalls otherwise.
    let _ = worker_vectorize::delete_ids(env, &[memory.id.as_str()]).await;

    Ok(format!(
        "✓ Forgotten: {} (id: {}). Tombstone recorded.",
        memory.summary,
        &memory.id[..8]
    ))
}

#[derive(Deserialize)]
struct ReflectArgs {
    conversation_highlights: String,
    #[serde(default)]
    memories_to_update: Vec<ReflectUpdate>,
}

#[derive(Deserialize)]
struct ReflectUpdate {
    memory_id: String,
    new_content: String,
    new_summary: String,
}

async fn tool_reflect(
    env: &Env,
    db: &D1Database,
    args: Value,
) -> std::result::Result<String, String> {
    let args: ReflectArgs =
        serde_json::from_value(args).map_err(|e| format!("invalid reflect args: {}", e))?;

    let mut updated = 0usize;
    let mut failed = 0usize;
    for update in &args.memories_to_update {
        let Some(memory) = worker_store::find_by_prefix(db, &update.memory_id)
            .await
            .map_err(|e| format!("find_by_prefix: {:?}", e))?
        else {
            failed += 1;
            continue;
        };
        match worker_store::reframe(db, &memory.id, &update.new_content, &update.new_summary)
            .await
        {
            Ok(true) => {
                if let Ok(emb) = worker_embed::embed_document(env, &update.new_content).await {
                    let _ = worker_vectorize::upsert_one(env, &memory.id, &emb).await;
                }
                updated += 1;
            }
            _ => failed += 1,
        }
    }

    // Write the reflection itself as a new episodic memory tagged "reflection".
    let summary_truncated: String = args
        .conversation_highlights
        .chars()
        .take(80)
        .collect::<String>();
    let reflection = worker_store::create_memory_with_provenance(
        db,
        MemoryType::Episodic,
        args.conversation_highlights.clone(),
        format!("Conversation reflection: {}", summary_truncated),
        None,
        vec!["reflection".to_string()],
        worker_auth_ctx::current_recorded_by(),
    )
    .await
    .map_err(|e| format!("create reflection memory: {:?}", e))?;

    let embedding = worker_embed::embed_document(env, &args.conversation_highlights)
        .await
        .map_err(|e| format!("embed reflection: {:?}", e))?;
    worker_vectorize::upsert_one(env, &reflection.id, &embedding)
        .await
        .map_err(|e| format!("vectorize upsert reflection: {:?}", e))?;

    Ok(format!(
        "✓ Reflection complete.\n  New episodic memory: {}\n  Updated: {}\n  Failed: {}",
        &reflection.id[..8],
        updated,
        failed
    ))
}

/// Same shape as native main.rs::format_memory — keeps the recall output
/// visually consistent across the migration window.
// Invalid-request and invalid-params codes are exported for completeness
// even though the current dispatch path doesn't surface them yet.
#[allow(dead_code)]
const _: i32 = INVALID_REQUEST;
#[allow(dead_code)]
const _: i32 = INVALID_PARAMS;
