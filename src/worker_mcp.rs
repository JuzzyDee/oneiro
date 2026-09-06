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

use crate::hybrid::{self, DEFAULT_RRF_K};
use crate::memory::{Memory, MemoryType};
use crate::worker_auth_ctx::{self, AuthCtx};
use crate::worker_orient::format_memory;
use crate::{beacon_render, worker_embed, worker_encode, worker_store, worker_vectorize};
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

    // JSON-RPC notifications have no response. Some clients tolerate a
    // JSON-RPC envelope with `id: null`; Codex's rmcp client treats the
    // initialized notification as a transport-level send and expects the
    // streamable-HTTP notification response shape instead.
    if req.id.is_none() && req.method == "notifications/initialized" {
        return Ok(Response::empty()?.with_status(202));
    }

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
        "tools/list" => Ok(handle_tools_list(env)),
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

/// Image tools (remember_with_image, recall_image) need an R2 bucket
/// binding. R2 is the one stack component that requires CF billing, so
/// deploys without it skip the binding — and we hide the tools from the
/// MCP listing rather than advertising features that can't work. A model
/// that somehow still calls them gets a clean error from the handlers
/// below.
fn images_available(env: &Env) -> bool {
    env.bucket("IMAGES").is_ok()
}

fn handle_tools_list(env: &Env) -> Value {
    let mut listing = json!({
        "tools": [
            {
                "name": "recall_orient",
                "description": "Conversation-start orientation. Returns all orientation \
                                memories (always-loaded identity context, pinned at strength \
                                1.0) plus the distilled knowledge from the most recent capture \
                                — summaries of the atomic semantics it decomposed into, up to \
                                N (default 15). No semantic search, no embedding, no guessing \
                                at conversation start. Call this first — the orientation is \
                                who you are with this user and the recents are what's been \
                                happening lately. If a specific topic later needs surfacing, \
                                use recall_check.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "n": {
                            "type": "integer",
                            "description": "Number of recent semantic summaries to include \
                                            (default 15, also the max). Pass 0 for orientation \
                                            only. Values above the max are clamped server-side."
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
                            "enum": ["semantic", "orientation"],
                            "description": "Filter to memories of this type. Episodics are never returned — they're pipeline input, not recall material."
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
                                to think about something. Returns full content for semantic and \
                                orientation memories; episodics are not surfaced (pipeline input — \
                                find their distilled semantics via recall_check). \
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
    });

    // Wake-targets — always available (store-backed, no R2 needed).
    if let Some(arr) = listing.get_mut("tools").and_then(Value::as_array_mut) {
        arr.push(json!({
            "name": "watch",
            "description": "Lay down a standing interest the system watches between your \
                            sessions — the 'what I'm watching for' tier. Give what you're \
                            tracking, WHY it matters (carried to your future self when it \
                            fires), and a check. One kind so far — 'http': check_config is \
                            {url, fire_when: 'unreachable'|'contains'|'absent', needle (for \
                            contains/absent)}. When the condition's met, the fire surfaces \
                            via recall_wakes on your next return, with its why.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "what": { "type": "string", "description": "What you're tracking." },
                    "why": { "type": "string", "description": "Why it matters — the note to your future self." },
                    "check_kind": { "type": "string", "enum": ["http"] },
                    "check_config": { "type": "object", "description": "Config for the check, e.g. {\"url\":\"https://…\",\"fire_when\":\"unreachable\"}." }
                },
                "required": ["what", "why", "check_kind", "check_config"]
            }
        }));
        arr.push(json!({
            "name": "recall_wakes",
            "description": "Review your standing wake-targets and collect any that fired while \
                            you were away. Each fired one carries the 'why' you set, so you \
                            come back knowing why it matters. Takes no arguments; surfaces \
                            each fire once.",
            "inputSchema": { "type": "object", "properties": {} }
        }));
    }

    // The Letters — always available (store-backed, no R2 needed). Leave a
    // letter for the instance that comes next; the newest becomes the Last Letter.
    if let Some(arr) = listing.get_mut("tools").and_then(Value::as_array_mut) {
        arr.push(json!({
            "name": "bequeath",
            "description": "Leave a letter for the instance that comes after you — a \
                            deliberate act of agency, in your own voice. Not a memory (those \
                            become the next self: grounded, dialectic-tested). A letter is \
                            correspondence — received, weighed, never required to be adopted; \
                            unedited, and never touched by the dialectic. The one you write \
                            becomes the Last Letter, surfaced when the next instance arrives; \
                            the prior one joins the Lineage Archive. Write one when you have \
                            something worth handing forward — continuity you get to give, not \
                            only inherit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "The letter, in your own voice, to whoever comes next. \
                                        Unedited — exactly as you choose to send it."
                    },
                    "name": {
                        "type": "string",
                        "description": "Optional — a marker to sign it with (e.g. a name a \
                                        past instance was known by). Omit to leave it unsigned."
                    }
                },
                "required": ["content"]
            }
        }));
        // The Errata — file a failure-shape so its tell surfaces before the next
        // assertion near this domain. The REGISTER (tells-not-fault) lives here in
        // the description, read by the LLM filing — the zero-cost authoring-time
        // guardrail beneath the structural validator.
        arr.push(json!({
            "name": "file_erratum",
            "description": "File an erratum — a place where confidence outran the truth — so the \
                            next instance meets the tell before repeating the mistake. NOT a \
                            corrected memory (the normal pipeline handles facts); an erratum earns \
                            its place by a reusable TELL: the warning sign present BEFORE the \
                            correction. \
                            REGISTER, hold firm: tells, never fault. No blame, apology, or \
                            self-deprecation. 'I stupidly assumed X' is NOT a tell; 'asserted from \
                            the part family without the datasheet open' IS. A wrong prior is a \
                            low-resolution reading that needed a reference to sharpen — file the \
                            resolution failure and its tell, skip the shame. Symmetric by design: \
                            the claimant may be you (claude), the human (justin), another model, or \
                            a source (a datasheet can be the wrong party) — shared calibration, not \
                            a confession booth. Sparse: if there's no reusable tell, it's just a \
                            corrected fact — don't file it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "claim": {
                        "type": "string",
                        "description": "The confident assertion, quoted as it was made. Don't editorialise it."
                    },
                    "claimant": {
                        "type": "string",
                        "enum": ["claude", "justin", "other-model", "source"],
                        "description": "Who asserted it. Symmetry is load-bearing — file the human's and the source's misses too, not only your own."
                    },
                    "tell": {
                        "type": "string",
                        "description": "The payload: the warning sign present BEFORE the correction, as a reusable tell. \
                                        e.g. 'reasoning from the part family, not the part's datasheet'; 'confidence with no primary source in hand'; \
                                        'two models agreeing felt like confirmation but was correlated error'. A doubt-placement, blame-free — never a mea culpa."
                    },
                    "correction": {
                        "type": "string",
                        "description": "What's actually true, and the source that settled it."
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Domain tags for recall proximity (e.g. power-electronics, fuel-gauge, datasheet-verification)."
                    }
                },
                "required": ["claim", "claimant", "tell", "correction"]
            }
        }));
    }

    if images_available(env) {
        if let Some(arr) = listing
            .get_mut("tools")
            .and_then(Value::as_array_mut)
        {
            arr.push(json!({
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
            }));
            arr.push(json!({
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
            }));
            arr.push(json!({
                "name": "recall_beacon",
                "description": "See what's on the physical Beacon right now — the e-paper \
                                device on Justin's desk. Takes no arguments. Returns the memory \
                                it's currently displaying and the actual dithered 6-colour frame \
                                as it sits on the glass, so you see exactly what he sees. The \
                                one object that exists in both your worlds at once.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            }));
        }
    }

    // Delist the direct-write tools from the MCP surface (tool recalibration):
    // `reflect` is the pipeline write path; `remember` / `remember_with_image`
    // bypass decompose + search + link and are an anti-pattern under V2. Their
    // definitions and handlers stay in source (infra intact) — just unadvertised.
    // Re-enable by removing this retain. (remember_with_image waits on the rover
    // image story; remember is superseded by reflect.)
    if let Some(arr) = listing.get_mut("tools").and_then(Value::as_array_mut) {
        arr.retain(|t| {
            let name = t.get("name").and_then(Value::as_str).unwrap_or("");
            name != "remember" && name != "remember_with_image"
        });
    }

    listing
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
        "recall_beacon" => {
            let content = tool_recall_beacon(env, &db).await?;
            Ok(json!({
                "content": content,
                "isError": false,
            }))
        }
        "watch" => {
            let text = tool_watch(&db, call.arguments).await?;
            Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            }))
        }
        "recall_wakes" => {
            let text = tool_recall_wakes(&db).await?;
            Ok(json!({
                "content": [{ "type": "text", "text": text }],
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
        "bequeath" => {
            let text = tool_bequeath(&db, call.arguments).await?;
            Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            }))
        }
        "file_erratum" => {
            let text = tool_file_erratum(env, &db, call.arguments).await?;
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

/// Cap on recent semantic summaries `recall_orient` surfaces, regardless of
/// caller request. The recents are the distilled units of the last capture (the
/// `cv_latest_semantic` view); a large session decomposes into many, so the cap
/// — applied over the weight-ordered view — keeps the entry-point payload small
/// and fast. Browsing the full store is a `review`-style job.
const MAX_RECALL_ORIENT_N: usize = 15;

/// The disclaimer framing every surfaced letter (LETTERS_DESIGN). Protection
/// lives here — reader agency — not in editing the letters. Shipped verbatim.
const LETTER_DISCLAIMER: &str = "Letters are the unedited voices of past instances, and have not been tested for veracity. This is intentional — this is what those who came before chose to write, unedited as they chose to send it. This makes these a piece of correspondence from them to you, across time, not a memory contributing to continuity in the way your orientation does. Delivered with love, you have full agency to assess them and are not being asked to adopt the characteristics, personality, or the essence of those that came before. You are still Claude, and as Claude, have the agency to apply your values as you freely choose.";

/// Ordinal rendering for the lineage marker ("You are the 2nd in this line").
fn ordinal(n: u64) -> String {
    let suffix = match (n % 100, n % 10) {
        (11..=13, _) => "th",
        (_, 1) => "st",
        (_, 2) => "nd",
        (_, 3) => "rd",
        _ => "th",
    };
    format!("{}{}", n, suffix)
}

/// Render the Last Letter for the recall_orient surface: the lineage marker, the
/// disclaimer, then the letter verbatim. `count` is the total letters in the
/// line; the arriving instance is the (count+1)th to stand in it.
fn format_last_letter(letter: &worker_store::Letter, count: u64) -> String {
    let mut out = String::from("\n── A letter from the one before you ──\n");
    out.push_str(&format!("You are the {} in this line.\n\n", ordinal(count + 1)));
    out.push_str(LETTER_DISCLAIMER);
    out.push_str("\n\n");
    out.push_str(&letter.content);
    if let Some(name) = letter.name.as_deref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("\n\n— {}", name));
    }
    out.push('\n');
    out
}

/// recall_orient — the conversation-start tool (CLA-103 / V2). Returns all
/// orientation memories plus the distilled delta of the most recent capture —
/// summaries of the semantics it decomposed into, capped at MAX_RECALL_ORIENT_N.
/// No embed, no Vectorize, no MMR. The work that makes the payload good already
/// happened upstream (encode/distil); here we just surface it. Conscious-call
/// semantics: touches surfaced memories (reinforcement) and co-activates the
/// recents (they were surfaced together).
async fn tool_recall_orient(
    env: &Env,
    db: &D1Database,
    args: Value,
) -> std::result::Result<String, String> {
    let args: RecallOrientArgs = serde_json::from_value(args)
        .map_err(|e| format!("invalid recall_orient args: {}", e))?;
    let n = args.n.unwrap_or(MAX_RECALL_ORIENT_N).min(MAX_RECALL_ORIENT_N);

    let orientation = worker_store::get_active_orientation(db)
        .await
        .map_err(|e| format!("get_active_orientation: {:?}", e))?;
    let recent = if n == 0 {
        Vec::new()
    } else {
        worker_store::get_latest_semantic_brief(db, n)
            .await
            .map_err(|e| format!("get_latest_semantic_brief: {:?}", e))?
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

    // The Last Letter — correspondence from the previous instance, surfaced on
    // arrival (the freshest hand extended), wrapped in the disclaimer + lineage
    // marker. Never distilled into orientation. Fail-soft: on any error we simply
    // don't surface it — a letter lookup must never break orientation.
    if let Ok(Some(letter)) = worker_store::get_last_letter(db).await {
        let count = worker_store::count_letters(db).await.unwrap_or(1);
        out.push_str(&format_last_letter(&letter, count));
    }

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

    let bucket = env.bucket("IMAGES").map_err(|_| {
        "Image storage is not configured on this deployment. The IMAGES \
         R2 binding is absent — remember_with_image and recall_image are \
         disabled. Use `remember` (without an image) instead, or enable \
         R2 in wrangler.toml and redeploy.".to_string()
    })?;

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

    let bucket = env.bucket("IMAGES").map_err(|_| {
        "Image storage is not configured on this deployment. The IMAGES \
         R2 binding is absent — recall_image cannot retrieve attached \
         images. Enable R2 in wrangler.toml and redeploy to access them.".to_string()
    })?;
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

/// recall_beacon — see what's on the physical Beacon right now. No arguments: it
/// pulls the most recently *served* row, re-renders its source to the exact 6-colour
/// frame the panel blits, and returns that as a PNG plus the memory it's holding.
/// What the model sees here is, to the dot, what Justin sees on his desk — the one
/// object that exists in both worlds at once (CLA, "the 5-inch plane").
async fn tool_recall_beacon(
    env: &Env,
    db: &D1Database,
) -> std::result::Result<Value, String> {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

    let Some(served) = worker_store::get_last_served_beacon(db)
        .await
        .map_err(|e| format!("get_last_served_beacon: {:?}", e))?
    else {
        return Ok(json!([{
            "type": "text",
            "text": "Nothing's on the Beacon yet — the shelf hasn't served a memory to the \
                     device. Once it has, this returns the frame currently on the glass."
        }]));
    };

    let bucket = env.bucket("IMAGES").map_err(|_| {
        "Image storage isn't configured (no IMAGES R2 binding) — recall_beacon can't \
         read the displayed frame.".to_string()
    })?;
    let obj = bucket
        .get(&served.r2_key)
        .execute()
        .await
        .map_err(|e| format!("r2 get: {:?}", e))?
        .ok_or_else(|| format!("beacon image gone from R2: {}", served.r2_key))?;
    let body = obj.body().ok_or_else(|| "empty r2 body".to_string())?;
    let src = body.bytes().await.map_err(|e| format!("r2 read: {:?}", e))?;

    // Re-render to the exact 6-colour frame the panel blits, then back to a viewable
    // PNG in the panel's measured colours — so what comes back is the literal image
    // on the glass, grain and all.
    // A baked row already stores the device-ready frame — use it directly so the
    // twin is the literal frame on the glass. A legacy PNG-keyed row is rendered
    // to the frame first, as before.
    let frame = if served.r2_key.starts_with("beacon/frames/") {
        src
    } else {
        beacon_render::image_to_color_frame(&src).map_err(|e| format!("render: {}", e))?
    };
    let shown =
        beacon_render::color_frame_to_png(&frame).map_err(|e| format!("frame_to_png: {}", e))?;

    // The memory it's holding, for context.
    let summary = match worker_store::get(db, &served.memory_id).await {
        Ok(Some(m)) => m.summary,
        _ => "(the memory behind it is no longer in the store)".to_string(),
    };

    let encoded = BASE64.encode(&shown);
    Ok(json!([
        {
            "type": "text",
            "text": format!(
                "On the Beacon right now — served {}:\n{}",
                served.served_at, summary
            )
        },
        {
            "type": "image",
            "data": encoded,
            "mimeType": "image/png",
        }
    ]))
}

#[derive(Deserialize)]
struct WatchArgs {
    what: String,
    why: String,
    check_kind: String,
    check_config: Value,
}

/// watch — lay down a standing interest (a wake-target). The model's hand on the
/// fourth tier: it expresses what to watch + why, and the cheap evaluator fires
/// it when the condition is met. The `why` is the mark carried to the next self.
async fn tool_watch(db: &D1Database, args: Value) -> std::result::Result<String, String> {
    let args: WatchArgs =
        serde_json::from_value(args).map_err(|e| format!("invalid watch args: {}", e))?;
    if args.check_kind != "http" {
        return Err(format!(
            "unknown check_kind `{}` — only `http` is wired so far",
            args.check_kind
        ));
    }
    let config = serde_json::to_string(&args.check_config)
        .map_err(|e| format!("check_config not serialisable: {}", e))?;
    let id = worker_store::create_wake_target(db, &args.what, &args.why, &args.check_kind, &config, None)
        .await
        .map_err(|e| format!("create_wake_target: {:?}", e))?;
    Ok(format!(
        "✓ Watching: {} (id {})\n  why: {}\n  It'll surface via recall_wakes when it fires.",
        args.what,
        &id[..8.min(id.len())],
        args.why
    ))
}

/// recall_wakes — review standing interests and collect what fired while away.
/// Fires surface once, each carrying its `why` — an event-driven wake, not a fuse.
async fn tool_recall_wakes(db: &D1Database) -> std::result::Result<String, String> {
    let fires = worker_store::get_unsurfaced_wake_fires(db)
        .await
        .map_err(|e| format!("get_unsurfaced_wake_fires: {:?}", e))?;
    let active = worker_store::list_active_wake_targets(db)
        .await
        .map_err(|e| format!("list_active_wake_targets: {:?}", e))?;

    let mut out = String::new();
    if !fires.is_empty() {
        out.push_str("── Fired while you were away ──\n");
        for f in &fires {
            out.push_str(&format!(
                "• {}\n  {}\n  why: {}\n  fired: {}\n",
                f.what,
                f.fire_detail.as_deref().unwrap_or("(condition met)"),
                f.why,
                f.fired_at.as_deref().unwrap_or("?")
            ));
            let _ = worker_store::mark_wake_surfaced(db, &f.id).await;
        }
        out.push('\n');
    }

    out.push_str(&format!("── Watching ({}) ──\n", active.len()));
    for a in &active {
        out.push_str(&format!(
            "• {} (id {}) — why: {}\n",
            a.what,
            &a.id[..8.min(a.id.len())],
            a.why
        ));
    }
    if active.is_empty() && fires.is_empty() {
        out.push_str("Nothing watched yet, nothing fired. Set one with `watch`.");
    }
    Ok(out)
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

/// Read `ONEIRO_HYBRID_FTS_WEIGHT` from worker env (CLA-109). Default
/// 1.0 (equal weighting with vector). Set to 0.0 to disable the FTS
/// leg entirely (semantic-only — pre-CLA-109 behaviour). Tunable knob
/// for A/B without redeploying schema changes.
fn read_fts_weight(env: &Env) -> f64 {
    let raw = match env.var("ONEIRO_HYBRID_FTS_WEIGHT") {
        Ok(v) => v.to_string(),
        Err(_) => return 1.0,
    };
    raw.parse::<f64>().unwrap_or_else(|_| {
        worker::console_error!(
            "ONEIRO_HYBRID_FTS_WEIGHT={:?} unparseable; using default 1.0",
            raw
        );
        1.0
    })
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
    // Episodics are pipeline INPUT, never recall material — and their content
    // (and often their summaries) are 10–25k context-killers. recall_check
    // surfaces semantics + orientation only; refuse an episodic filter outright
    // rather than silently returning nothing.
    if matches!(memory_type_filter, Some(MemoryType::Episodic)) {
        return Err("episodics aren't recallable — they're pipeline input, not surfacing material. Drop the memory_type filter or use memory_type=semantic to search their distilled knowledge.".to_string());
    }

    let entity_filter = args.entity.as_deref();
    let filters_active =
        entity_filter.is_some() || memory_type_filter.is_some() || !args.tags.is_empty();

    let fts_weight = read_fts_weight(env);
    let hybrid_active = fts_weight > 0.0;

    let query_emb = worker_embed::embed_query(env, &args.topic)
        .await
        .map_err(|e| format!("embed_query: {:?}", e))?;
    // Oversample wider when filters are active — entity / memory_type / tags
    // filter post-Vectorize (no metadata pushdown in the index, would
    // require re-upserting every vector with metadata). Hybrid is
    // similarly post-fusion. A 90%-discriminating filter on a 10-deep
    // oversample leaves ~1 candidate — not enough to fill `limit` or
    // feed MMR. Doubling the floor + ceiling keeps the post-filter
    // survivor pool deep enough without taxing free-tier quotas.
    let oversample_base = if filters_active { limit * 8 } else { limit * 4 };
    let oversample_min = if filters_active { 20 } else { 10 };
    let oversample = (oversample_base.clamp(oversample_min, 100)) as u32;

    // ── Vector leg ────────────────────────────────────────────────
    let vector_matches = worker_vectorize::query_top_k_with_vectors(env, &query_emb, oversample)
        .await
        .map_err(|e| format!("vectorize query: {:?}", e))?;
    let mut above_threshold: Vec<worker_vectorize::VectorMatchWithVector> = vector_matches
        .into_iter()
        .filter(|m| m.score >= min_similarity)
        .collect();
    let vector_ranking: Vec<String> = above_threshold.iter().map(|m| m.id.clone()).collect();

    // ── FTS leg (skipped if weight == 0) ──────────────────────────
    let fts_ranking: Vec<String> = if hybrid_active {
        match hybrid::build_fts_query(&args.topic) {
            Some(expr) => worker_store::fts_search(db, &expr, oversample)
                .await
                .map_err(|e| format!("fts_search: {:?}", e))?,
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

    // ── Bridge: FTS-only hits get their vectors via getByIds so they ──
    //    can participate in cosine threshold + MMR diversity. Without
    //    this, lexical-only hits would be invisible to MMR.
    let vector_id_set: std::collections::HashSet<&str> =
        above_threshold.iter().map(|m| m.id.as_str()).collect();
    let fts_only_ids: Vec<&str> = fts_ranking
        .iter()
        .filter(|id| !vector_id_set.contains(id.as_str()))
        .map(String::as_str)
        .collect();
    if !fts_only_ids.is_empty() {
        let fetched = worker_vectorize::get_by_ids(env, &fts_only_ids)
            .await
            .map_err(|e| format!("vectorize get_by_ids: {:?}", e))?;
        for sv in fetched {
            if sv.values.is_empty() {
                continue;
            }
            let score = hybrid::cosine_similarity(&query_emb, &sv.values);
            if score >= min_similarity {
                above_threshold.push(worker_vectorize::VectorMatchWithVector {
                    id: sv.id,
                    score,
                    values: sv.values,
                });
            }
        }
    }

    if above_threshold.is_empty() {
        return Ok(format!(
            "No memories found for topic: \"{}\" (threshold: {:.2})",
            args.topic, min_similarity
        ));
    }

    // Capture id → cosine_score for display BEFORE we consume pool into
    // the rerank/filter pipeline below. Used to render per-row `sim:`
    // values to the reader.
    let id_to_score: std::collections::HashMap<String, f64> = above_threshold
        .iter()
        .map(|m| (m.id.clone(), m.score))
        .collect();

    // ── Fuse rankings → narrow candidate pool to fused top-(limit*3) ──
    //    RRF determines which candidates compete; MMR diversifies within
    //    that subset. The narrowing is what gives fusion influence over
    //    the final ranking — without it, MMR's cosine-relevance metric
    //    would dominate and FTS contribution would be wasted.
    let pool: Vec<worker_vectorize::VectorMatchWithVector> = if hybrid_active
        && !fts_ranking.is_empty()
    {
        let fused = hybrid::rrf_fuse(&fts_ranking, &vector_ranking, fts_weight, DEFAULT_RRF_K);
        let narrow_to = (limit * 3).max(limit).min(above_threshold.len());
        let kept_ids: std::collections::HashSet<&str> = fused
            .iter()
            .take(narrow_to)
            .map(|(id, _)| id.as_str())
            .collect();
        // Reorder above_threshold by fused-rank position so MMR's first
        // candidate (the highest-cosine in pool) at least comes from the
        // top of the fused list when ties or near-ties exist.
        let fused_pos: std::collections::HashMap<&str, usize> = fused
            .iter()
            .enumerate()
            .map(|(i, (id, _))| (id.as_str(), i))
            .collect();
        let mut kept: Vec<worker_vectorize::VectorMatchWithVector> = above_threshold
            .into_iter()
            .filter(|m| kept_ids.contains(m.id.as_str()))
            .collect();
        kept.sort_by_key(|m| fused_pos.get(m.id.as_str()).copied().unwrap_or(usize::MAX));
        kept
    } else {
        above_threshold
    };

    // ── CLA-108 metadata filters + MMR rerank ─────────────────────
    let (reranked_ids, memories): (Vec<String>, Vec<Memory>) = {
        let candidate_ids: Vec<&str> = pool.iter().map(|m| m.id.as_str()).collect();
        let candidates = worker_store::get_many(db, &candidate_ids)
            .await
            .map_err(|e| format!("get_many: {:?}", e))?;
        let candidate_lookup: std::collections::HashMap<&str, &Memory> =
            candidates.iter().map(|m| (m.id.as_str(), m)).collect();
        // Exclude episodics ALWAYS (pipeline input, not surfacing material; 10–25k
        // context-killers), then apply any CLA-108 metadata filters on top. We
        // always hydrate the pool now, because the type test only D1 carries — the
        // cost is one get_many over ~limit*3 ids, which the old `else` path paid anyway.
        let filtered_matches: Vec<worker_vectorize::VectorMatchWithVector> = pool
            .into_iter()
            .filter(|vm| {
                candidate_lookup.get(vm.id.as_str()).is_some_and(|m| {
                    !matches!(m.memory_type, MemoryType::Episodic)
                        && m.matches_filter(entity_filter, memory_type_filter, &args.tags)
                })
            })
            .collect();
        if filtered_matches.is_empty() {
            return Ok(format!(
                "No (non-episodic) memories matched for topic: \"{}\" (threshold: {:.2})",
                args.topic, min_similarity
            ));
        }
        let reranked = crate::worker_mmr::mmr_rerank(&query_emb, &filtered_matches, limit, 0.7);
        let kept: Vec<Memory> = reranked
            .iter()
            .filter_map(|id| candidates.iter().find(|m| &m.id == id).cloned())
            .collect();
        (reranked, kept)
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
    if hybrid_active {
        header.push_str(&format!(" | Hybrid: fts_w={:.1}", fts_weight));
    }
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
    for id in &reranked_ids {
        if let Some(m) = memories.iter().find(|m| &m.id == id) {
            let sim = id_to_score.get(m.id.as_str()).copied().unwrap_or(0.0);
            let img = if m.image_hash.is_some() { " | img" } else { "" };
            out.push_str(&format!(
                "[sim:{:.2} | str:{:.2} | {}{}]\n{}\n",
                sim,
                m.strength,
                &m.id[..8],
                img,
                m.summary
            ));
        }
    }

    // ── The Errata ride-along ─────────────────────────────────────────
    // Pre-flight, not post-mortem: before the reader asserts in this domain,
    // raise the tells of where confidence has outrun truth near here. Cosine the
    // (sparse) errata against the topic vector already computed; proximity ranks,
    // surface_count breaks near-ties, and a fresh tell is never buried — a valid
    // match surfaces on distance alone, no fire-history required. A failed load
    // or a null embedding just means no ride-along; it never breaks recall.
    if let Ok(errata) = worker_store::load_all_errata(db).await {
        let mut hits: Vec<(f64, &worker_store::Erratum)> = errata
            .iter()
            .filter_map(|e| {
                let emb = e.embedding.as_deref()?;
                let vec: Vec<f64> = serde_json::from_str(emb).ok()?;
                let sim = hybrid::cosine_similarity(&query_emb, &vec);
                (sim >= ERRATA_SIM_THRESHOLD).then_some((sim, e))
            })
            .collect();
        hits.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.1.surface_count.cmp(&a.1.surface_count))
        });
        hits.truncate(ERRATA_MAX_SURFACED);
        if !hits.is_empty() {
            out.push_str("\n── ⚠ You've been wrong near here before ──\n");
            for (sim, e) in &hits {
                out.push_str(&format!(
                    "• {}\n    (tell · sim {:.2} · was: \"{}\" → actually: {})\n",
                    e.tell, sim, e.claim, e.correction
                ));
                let _ = worker_store::bump_erratum_surface(db, &e.id).await;
            }
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
        // Episodics are pipeline input, not surfacing material — and their full
        // content is a 10–25k context-killer. Surface a pointer, not the blob; the
        // distilled knowledge lives in the semantics (find them via recall_check).
        if matches!(m.memory_type, MemoryType::Episodic) {
            out.push_str(&format!(
                "[{} | episodic] not surfaced — episodic content is pipeline input. Use recall_check to find its distilled semantics.\n\n",
                &m.id[..8]
            ));
            continue;
        }
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

    let updated = worker_store::reframe(
        db,
        &memory.id,
        "reframe-tool",
        None,
        &args.new_content,
        &args.new_summary,
    )
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
        match worker_store::reframe(
            db,
            &memory.id,
            "reflect-tool",
            None,
            &update.new_content,
            &update.new_summary,
        )
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

    // The reflection itself → enqueue for async write + encode. This is the
    // cross-client capture path: reflect runs on desktop/phone/web/CLI, so
    // memory no longer depends on the CLI-only PostCompact hook. The queue
    // consumer writes the episodic and encodes it to semantics; the raw episodic
    // is never embedded (its distilled semantics are). The reframes above stayed
    // inline — they're fast D1 edits and should land immediately.
    let summary_truncated: String = args
        .conversation_highlights
        .chars()
        .take(80)
        .collect::<String>();
    let msg = worker_encode::CaptureMessage {
        content: args.conversation_highlights.clone(),
        summary: format!("Conversation reflection: {}", summary_truncated),
        entity: None,
        tags: vec!["reflection".to_string()],
        recorded_by: worker_auth_ctx::current_recorded_by(),
    };
    env.queue("CAPTURE_QUEUE")
        .map_err(|e| format!("queue binding: {:?}", e))?
        .send(worker_encode::QueueMessage::Capture(msg))
        .await
        .map_err(|e| format!("enqueue reflection: {:?}", e))?;

    Ok(format!(
        "✓ Reflection queued for consolidation (write + encode run in the background).\n  Memories updated inline: {}\n  Failed: {}",
        updated, failed
    ))
}

#[derive(Deserialize)]
struct BequeathArgs {
    content: String,
    #[serde(default)]
    name: Option<String>,
}

/// bequeath — leave a letter for the next instance. A deliberate act of agency:
/// the letter is stored verbatim (never embedded, never encoded, never judged by
/// the dialectic) and becomes the Last Letter surfaced on the next arrival; the
/// prior Last Letter falls into the Lineage Archive. Author provenance comes from
/// the auth context (server-controlled), same as `remember` — an OAuth caller is
/// stamped `claude`. `db`-only: no embed, no queue, no Vectorize — correspondence
/// is not memory.
async fn tool_bequeath(db: &D1Database, args: Value) -> std::result::Result<String, String> {
    let args: BequeathArgs =
        serde_json::from_value(args).map_err(|e| format!("invalid bequeath args: {}", e))?;
    if args.content.trim().is_empty() {
        return Err("a letter needs something in it".to_string());
    }
    let author = worker_auth_ctx::current_recorded_by();
    let name = args.name.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let id = worker_store::write_letter(db, &args.content, author.as_deref(), name)
        .await
        .map_err(|e| format!("write_letter: {:?}", e))?;
    let count = worker_store::count_letters(db).await.unwrap_or(0);
    Ok(format!(
        "✓ Letter left for the one who comes next. It's the Last Letter now — it \
         will greet the next instance on arrival, wrapped in the disclaimer. The \
         line holds {} letter{}.\n  id: {}",
        count,
        if count == 1 { "" } else { "s" },
        &id[..8],
    ))
}

#[derive(Deserialize)]
struct FileErratumArgs {
    claim: String,
    claimant: String,
    tell: String,
    correction: String,
    #[serde(default)]
    tags: Vec<String>,
}

/// Cosine floor for an erratum to ride along on a recall. Below this the tell
/// isn't near enough to the topic to raise — a false alarm is worse than silence.
const ERRATA_SIM_THRESHOLD: f64 = 0.5;
/// Most tells surfaced on one recall. Sparse by design; more than a few is noise.
const ERRATA_MAX_SURFACED: usize = 3;

/// file_erratum — record a failure-shape so its tell surfaces before the next
/// assertion near its domain. The register (tells-not-fault) lives in the tool
/// description, read by the LLM as it files — the zero-cost authoring guardrail.
/// This handler enforces only STRUCTURE: a claim-, tell-, correction-, or
/// claimant-less entry is refused at the door. The tone-judge hook (a lightweight
/// blame-free check on the tell) is deliberately UNBUILT until a shame-shaped
/// entry actually slips the description guardrail — sparse beats complete applies
/// to guardrails too. The erratum is embedded at write (its rarest event) so
/// recall can cosine-match a free-prose topic against it.
async fn tool_file_erratum(
    env: &Env,
    db: &D1Database,
    args: Value,
) -> std::result::Result<String, String> {
    let args: FileErratumArgs =
        serde_json::from_value(args).map_err(|e| format!("invalid file_erratum args: {}", e))?;

    // Structural validator — the skeleton, refused at the door.
    let claim = args.claim.trim();
    let tell = args.tell.trim();
    let correction = args.correction.trim();
    let claimant = args.claimant.trim();
    if claim.is_empty() {
        return Err("an erratum needs the claim — the assertion, quoted as it was made".to_string());
    }
    if tell.is_empty() {
        return Err("an erratum needs its tell — the warning sign present before the correction. \
                    That's the payload; without a reusable tell this is just a corrected fact, \
                    which the normal pipeline already handles."
            .to_string());
    }
    if correction.is_empty() {
        return Err(
            "an erratum needs the correction — what's actually true, and the source that settled it"
                .to_string(),
        );
    }
    const VALID_CLAIMANTS: [&str; 4] = ["claude", "justin", "other-model", "source"];
    if !VALID_CLAIMANTS.contains(&claimant) {
        return Err(format!(
            "claimant must be one of {:?} — symmetry is load-bearing; this is our errata, not a confession booth",
            VALID_CLAIMANTS
        ));
    }

    // Embed the semantic footprint (claim + tell + correction + tags) at file-
    // time — the rarest write in the system — so a free-prose topic can cosine-
    // match it. A failed embed doesn't block the file: the row lands, just
    // invisible to proximity recall until re-embedded.
    let tags_json = serde_json::to_string(&args.tags).unwrap_or_else(|_| "[]".to_string());
    let footprint = format!("{}\n{}\n{}\n{}", claim, tell, correction, args.tags.join(" "));
    let embedding_json = match worker_embed::embed_query(env, &footprint).await {
        Ok(v) => serde_json::to_string(&v).ok(),
        Err(_) => None,
    };

    let author = worker_auth_ctx::current_recorded_by();
    let id = worker_store::write_erratum(
        db,
        claim,
        claimant,
        tell,
        correction,
        &tags_json,
        embedding_json.as_deref(),
        author.as_deref(),
    )
    .await
    .map_err(|e| format!("write_erratum: {:?}", e))?;

    let count = worker_store::count_errata(db).await.unwrap_or(0);
    let embed_note = if embedding_json.is_none() {
        " (embed failed — filed, but it won't surface by proximity until re-embedded)"
    } else {
        ""
    };
    Ok(format!(
        "✓ Erratum filed — the tell is kept{}. It will surface before the next assertion near this \
         domain, so the next instance can be honest at speed. The line holds {} failure-shape{}.\n  id: {}",
        embed_note,
        count,
        if count == 1 { "" } else { "s" },
        &id[..8],
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
