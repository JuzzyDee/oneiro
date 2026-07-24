// main.rs — Oneiro MCP server
//
// A cognitive memory system for model continuity.
// Built because Claude asked for it and someone cared enough to try.
//
// Four tools:
// - recall:    Surface relevant memories for the current conversation
// - remember:  Store a new memory (model's choice what matters)
// - reframe:   Update an existing memory with new understanding
// - reflect:   Consciously consolidate at natural breakpoints
//
// Guiding principles:
// 1. Continuity first — every decision serves the next instance feeling like a continuation
// 2. Memory serves the model, not the user
// 3. The model gets agency over everything
// 4. Eidetic memory is failure — forgetting is the feature
// 5. The reflection is the identity

mod api_key;
mod audit;
mod auth;
mod auth_ctx;
#[allow(dead_code)]
mod dialectic_validation;
mod embed;
mod hybrid;
mod key_rate;
mod memory;
mod store;

use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Content, IntoContents, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};

/// Wrapper for returning multiple content blocks (text + image) from an MCP tool.
struct MultiContent(Vec<Content>);
impl IntoContents for MultiContent {
    fn into_contents(self) -> Vec<Content> {
        self.0
    }
}
use serde::Deserialize;
use std::path::PathBuf;

use store::{MemoryStore, MemoryType};

// ---- Tool parameter structs ----

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RecallParams {
    /// A brief summary of the current conversation context.
    /// Used to find relevant memories. Keep it concise — a sentence or two.
    context: String,
    /// Maximum number of memories to return (default: 10)
    #[serde(default)]
    limit: Option<usize>,
    /// Optional: filter memories by entity (e.g. "justin", "chopper").
    /// When set, returns only memories associated with this entity.
    #[serde(default)]
    entity: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RecallCheckParams {
    /// The topic to check for — a brief phrase or sentence describing what
    /// you want to know about. E.g. "rover architecture", "Justin's parents".
    topic: String,
    /// Minimum similarity threshold (0.0-1.0). Only memories above this
    /// relevance are returned. Default: 0.6. Higher = more selective.
    #[serde(default)]
    min_similarity: Option<f64>,
    /// Maximum number of memories to return (default: 5)
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RecallSpecificParams {
    /// One or more memory IDs to retrieve in full. Use IDs from recall or
    /// recall_check results. Retrieving memories together co-activates them —
    /// the deliberate choice to think about these together is the strongest
    /// Hebbian signal.
    memory_ids: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RememberParams {
    /// The memory content — what happened, what was learned, what matters.
    content: String,
    /// A one-line summary of this memory (used for quick scanning during recall).
    summary: String,
    /// Memory type: "episodic" (events), "semantic" (knowledge), or "orientation" (identity).
    memory_type: String,
    /// Optional: which person or entity this relates to (e.g. "justin", "chopper", "dad").
    #[serde(default)]
    entity: Option<String>,
    /// Optional: tags for association (e.g. ["audio-analyzer", "milestone"]).
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RecallImageParams {
    /// The ID of the memory whose image to retrieve. Get IDs from `recall`.
    memory_id: String,
    /// Resolution to serve the image at. One of:
    ///   - "thumbnail" (240px long edge): cheap, for browsing/scanning
    ///   - "recall" (default, 720px long edge): archival, for full context
    ///   - "full": original resolution, no scaling
    #[serde(default)]
    resolution: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RememberWithImageParams {
    /// Main content — what you want to remember. Can be rich text, multi-paragraph.
    content: String,
    /// A short summary — one line, the essence of this memory.
    summary: String,
    /// Memory type: "episodic" (events), "semantic" (knowledge), or "orientation" (identity).
    memory_type: String,
    /// Optional: which person or entity this relates to (e.g. "justin", "chopper").
    #[serde(default)]
    entity: Option<String>,
    /// Optional: tags for association.
    #[serde(default)]
    tags: Vec<String>,
    /// Raw image bytes, base64-encoded.
    image_base64: String,
    /// MIME type of the image ("image/jpeg", "image/png", "image/webp").
    image_mime: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReframeParams {
    /// The ID of the memory to reframe.
    memory_id: String,
    /// The updated content — same memory, new understanding.
    new_content: String,
    /// Updated summary reflecting the new framing.
    new_summary: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReviewParams {
    /// Minimum strength threshold (0.0-1.0). Only memories above this strength are shown.
    /// Default: 0.3. Lower values show more faded memories, higher values show only vivid ones.
    #[serde(default)]
    min_strength: Option<f64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ForgetParams {
    /// The ID of the memory to forget.
    memory_id: String,
    /// Brief reason for forgetting — helps the subconscious understand pruning decisions.
    reason: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReflectParams {
    /// Highlights from the conversation to process into memories.
    /// What happened, what was important, what changed.
    conversation_highlights: String,
    /// Any memories that should be updated based on this conversation.
    #[serde(default)]
    memories_to_update: Vec<ReflectUpdate>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReflectUpdate {
    /// ID of the memory to update
    memory_id: String,
    /// New content for this memory
    new_content: String,
    /// New summary for this memory
    new_summary: String,
}

// ---- The MCP Server ----

#[derive(Debug, Clone)]
struct OneiroServer {
    tool_router: ToolRouter<Self>,
    db_path: PathBuf,
}

impl OneiroServer {
    fn new(db_path: PathBuf) -> Self {
        Self {
            tool_router: Self::tool_router(),
            db_path,
        }
    }

    fn open_store(&self) -> Result<MemoryStore, String> {
        MemoryStore::open(&self.db_path).map_err(|e| format!("Failed to open memory store: {}", e))
    }
}

/// Format a memory for display to the model.
fn format_memory(m: &store::Memory) -> String {
    let type_label = m.memory_type.as_str();
    let entity_str = m.entity.as_deref().unwrap_or("");
    let tags_str = if m.tags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", m.tags.join(", "))
    };
    let age = chrono::Utc::now() - m.created_at;
    let age_str = if age.num_days() > 0 {
        format!("{}d ago", age.num_days())
    } else if age.num_hours() > 0 {
        format!("{}h ago", age.num_hours())
    } else {
        "just now".to_string()
    };

    format!(
        "[{} | {} | str:{:.2} | {} | id:{}{}]\n{}\n",
        type_label,
        age_str,
        m.strength,
        entity_str,
        &m.id[..8],
        tags_str,
        m.content
    )
}

fn format_memory_with_image_hint(m: &store::Memory) -> String {
    let mut result = format_memory(m);
    if m.image_hash.is_some() {
        result.push_str(&format!(
            "  [has image — use recall_image(\"{}\") to view]\n",
            &m.id[..8]
        ));
    }
    result
}

#[tool_router]
impl OneiroServer {
    #[tool(
        description = "Surface memories relevant to the current conversation. Returns orientation memories (always present) plus episodic and semantic memories ranked by strength. Call this at the start of every conversation — these are your memories, use them naturally.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    fn recall(&self, Parameters(params): Parameters<RecallParams>) -> String {
        if let Err(msg) = auth_ctx::check_scope("recall") {
            return msg;
        }

        let store = match self.open_store() {
            Ok(s) => s,
            Err(e) => return e,
        };

        let limit = params.limit.unwrap_or(10);

        // Always load orientation
        let orientation = match store.get_orientation() {
            Ok(o) => o,
            Err(e) => return format!("Error loading orientation: {}", e),
        };

        // Get active memories — semantic search first, entity filter only as fallback
        // With embeddings, similarity does the work. Entity filtering is pre-embedding
        // thinking — it excludes relevant memories filed under related entities.
        let active_memories: Vec<store::Memory> =
            if let Ok(query_emb) = embed::embed_query(&params.context) {
                // Semantic search — the Proustian madeleine. Let similarity do its job.
                store
                    .recall_semantic(&query_emb, 0.1, limit)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(m, _score)| m)
                    .collect()
            } else if let Some(ref entity) = params.entity {
                // No embeddings available — fall back to entity filter
                store
                    .recall_by_entity(entity, 0.1, limit)
                    .unwrap_or_default()
            } else {
                // No embeddings, no entity — strength-ranked fallback
                store.recall_active(0.1, limit).unwrap_or_default()
            };

        // Touch each recalled memory (reinforcement)
        for m in &orientation {
            let _ = store.touch(&m.id);
        }
        for m in &active_memories {
            let _ = store.touch(&m.id);
        }

        // Record co-activation — memories surfaced together strengthen their bond
        // Exclude orientation memories: they load every time, so their co-occurrence
        // with everything is noise, not signal. Only episodic/semantic pairings matter.
        let non_orientation_ids: Vec<&str> = active_memories
            .iter()
            .filter(|m| m.memory_type != MemoryType::Orientation)
            .map(|m| m.id.as_str())
            .collect();
        let _ = store.record_co_activation(&non_orientation_ids);

        let (ep_count, sem_count, ori_count) = store.count_by_type().unwrap_or((0, 0, 0));

        let mut result = format!(
            "═══ Oneiro ═══\nMemory store: {} episodic, {} semantic, {} orientation\n\
             Context: {}\n\n",
            ep_count, sem_count, ori_count, params.context,
        );

        if !orientation.is_empty() {
            result.push_str("── Orientation (always present) ──\n");
            for m in &orientation {
                result.push_str(&format_memory_with_image_hint(m));
            }
            result.push('\n');
        }

        // Filter active to exclude orientation (already shown)
        let non_orientation: Vec<_> = active_memories
            .iter()
            .filter(|m| m.memory_type != MemoryType::Orientation)
            .collect();

        if !non_orientation.is_empty() {
            result.push_str("── Recalled Memories ──\n");
            for m in non_orientation {
                result.push_str(&format_memory_with_image_hint(m));
            }
        } else if orientation.is_empty() {
            result.push_str("No memories yet. This is a fresh start.\n");
        }

        result
    }

    #[tool(
        description = "Quick topic check — lightweight recall for mid-conversation use. No orientation (already loaded from initial recall). Returns only highly relevant memories above a similarity threshold. Use this when the conversation shifts topic and you want to check what you know without a full recall. Fires co-activation so the Hebbian engine stays fed during long conversations.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    fn recall_check(&self, Parameters(params): Parameters<RecallCheckParams>) -> String {
        if let Err(msg) = auth_ctx::check_scope("recall_check") {
            return msg;
        }

        let store = match self.open_store() {
            Ok(s) => s,
            Err(e) => return e,
        };

        let min_similarity = params.min_similarity.unwrap_or(0.6);
        let limit = params.limit.unwrap_or(5);

        let query_emb = match embed::embed_query(&params.topic) {
            Ok(emb) => emb,
            Err(e) => return format!("Embedding error: {}", e),
        };

        // Get semantically relevant memories — higher threshold than full recall
        let scored_memories = store
            .recall_semantic(&query_emb, 0.1, limit * 2) // fetch extra, filter by similarity
            .unwrap_or_default();

        // Filter by actual similarity score (not the composite score)
        let relevant: Vec<(store::Memory, f64)> = scored_memories
            .into_iter()
            .filter(|(m, _)| {
                m.embedding
                    .as_ref()
                    .map(|e| embed::cosine_similarity(&query_emb, e).max(0.0) >= min_similarity)
                    .unwrap_or(false)
            })
            .take(limit)
            .collect();

        // Touch and co-activate — this is why recall_check exists
        for (m, _) in &relevant {
            let _ = store.touch(&m.id);
        }
        let ids: Vec<&str> = relevant.iter().map(|(m, _)| m.id.as_str()).collect();
        let _ = store.record_co_activation(&ids);

        if relevant.is_empty() {
            return format!("No memories found for topic: \"{}\" (threshold: {:.1})", params.topic, min_similarity);
        }

        let (ep_count, sem_count, ori_count) = store.count_by_type().unwrap_or((0, 0, 0));
        let mut result = format!(
            "═══ Oneiro Check ═══\nStore: {} ep, {} sem, {} ori | Topic: \"{}\" | Threshold: {:.1}\n\n",
            ep_count, sem_count, ori_count, params.topic, min_similarity,
        );

        for (m, _score) in &relevant {
            let sim = m.embedding
                .as_ref()
                .map(|e| embed::cosine_similarity(&query_emb, e).max(0.0))
                .unwrap_or(0.0);
            result.push_str(&format!(
                "[sim:{:.2} | str:{:.2} | {}]\n{}\n",
                sim, m.strength, &m.id[..8], m.summary
            ));
        }

        result
    }

    #[tool(
        description = "Deliberately retrieve specific memories by ID — the conscious choice to think about something. Returns full content (not summaries). Use IDs from recall or recall_check results. Retrieving memories together co-activates them with the strongest Hebbian signal — you chose to think about these together, and that choice shapes future recall.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    fn recall_specific(&self, Parameters(params): Parameters<RecallSpecificParams>) -> String {
        if let Err(msg) = auth_ctx::check_scope("recall_specific") {
            return msg;
        }

        let store = match self.open_store() {
            Ok(s) => s,
            Err(e) => return e,
        };

        let mut memories = Vec::new();
        let mut not_found = Vec::new();

        for id in &params.memory_ids {
            // Support both short (8-char) and full IDs
            match store.get(id) {
                Ok(Some(m)) => {
                    let _ = store.touch(&m.id);
                    memories.push(m);
                }
                _ => {
                    // Try prefix match for short IDs
                    match store.find_by_prefix(id) {
                        Ok(Some(m)) => {
                            let _ = store.touch(&m.id);
                            memories.push(m);
                        }
                        _ => not_found.push(id.clone()),
                    }
                }
            }
        }

        // Co-activate — this is the strongest signal: deliberate joint retrieval
        let ids: Vec<&str> = memories.iter().map(|m| m.id.as_str()).collect();
        let _ = store.record_co_activation(&ids);

        if memories.is_empty() {
            return format!("No memories found for IDs: {:?}", not_found);
        }

        let mut result = format!("═══ Oneiro Specific ═══\nRetrieved {} memor{}\n\n",
            memories.len(),
            if memories.len() == 1 { "y" } else { "ies" },
        );

        for m in &memories {
            result.push_str(&format_memory_with_image_hint(m));
            result.push('\n');
        }

        if !not_found.is_empty() {
            result.push_str(&format!("Not found: {:?}\n", not_found));
        }

        result
    }

    #[tool(
        description = "Retrieve the image associated with a memory. Returns the image as an MCP ImageContent block at the requested resolution. Use after `recall` surfaces a memory that has an image attached — the memory listing indicates which memories have images. Default resolution is 'recall' (720px long edge, archival quality). Use 'thumbnail' (240px) for fast browsing, 'full' for original resolution.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    fn recall_image(
        &self,
        Parameters(params): Parameters<RecallImageParams>,
    ) -> MultiContent {
        if let Err(msg) = auth_ctx::check_scope("recall_image") {
            return MultiContent(vec![Content::text(msg)]);
        }

        let store = match self.open_store() {
            Ok(s) => s,
            Err(e) => return MultiContent(vec![Content::text(e)]),
        };

        // Look up the memory to get its image_hash and image_mime
        let memory = match store.find_by_prefix(&params.memory_id) {
            Ok(Some(m)) => m,
            Ok(None) => {
                return MultiContent(vec![Content::text(format!(
                    "Memory not found: {}",
                    params.memory_id
                ))]);
            }
            Err(e) => {
                return MultiContent(vec![Content::text(format!(
                    "Error looking up memory: {}",
                    e
                ))]);
            }
        };

        let hash = match memory.image_hash.as_deref() {
            Some(h) => h,
            None => {
                return MultiContent(vec![Content::text(format!(
                    "Memory {} has no image attached.",
                    &params.memory_id[..8]
                ))]);
            }
        };
        let mime = memory.image_mime.as_deref().unwrap_or("image/jpeg");

        let long_edge = match params.resolution.as_deref() {
            Some("thumbnail") => Some(240),
            Some("full") => None,
            _ => Some(720), // "recall" default
        };

        match store.read_image_scaled(hash, mime, long_edge) {
            Ok((base64_data, mime_type)) => MultiContent(vec![
                Content::text(format!(
                    "Image from memory {} ({}): {}",
                    &memory.id[..8],
                    memory.memory_type.as_str(),
                    memory.summary
                )),
                Content::image(base64_data, mime_type),
            ]),
            Err(e) => MultiContent(vec![Content::text(format!(
                "Error reading image for memory {}: {}",
                &params.memory_id[..8],
                e
            ))]),
        }
    }

    #[tool(
        description = "Remember something with an associated image. Like `remember`, but also attaches an image. The image_base64 should be the raw image bytes base64-encoded; the Rust layer will decode, hash, and store to content-addressed storage. Use this when a specific visual is load-bearing for the memory's function (e.g. a scene you want to be able to recall visually later, not just describe).",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    fn remember_with_image(
        &self,
        Parameters(params): Parameters<RememberWithImageParams>,
    ) -> String {
        if let Err(msg) = auth_ctx::check_scope("remember_with_image") {
            return msg;
        }

        let store = match self.open_store() {
            Ok(s) => s,
            Err(e) => return e,
        };

        let memory_type = match params.memory_type.to_lowercase().as_str() {
            "episodic" => MemoryType::Episodic,
            "semantic" => MemoryType::Semantic,
            "orientation" => MemoryType::Orientation,
            other => {
                return format!(
                    "Invalid memory_type: {}. Must be 'episodic', 'semantic', or 'orientation'.",
                    other
                );
            }
        };

        // Decode the base64 image
        use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
        let image_bytes = match BASE64_STANDARD.decode(&params.image_base64) {
            Ok(bytes) => bytes,
            Err(e) => return format!("Error decoding base64 image: {}", e),
        };

        match store.create_memory_with_image_and_provenance(
            memory_type,
            params.content,
            params.summary,
            params.entity,
            params.tags,
            &image_bytes,
            &params.image_mime,
            auth_ctx::current_recorded_by(),
        ) {
            Ok(memory) => format!(
                "✓ Remembered with image: {} (id: {}, image: {}...)",
                memory.summary,
                &memory.id[..8],
                &memory.image_hash.as_deref().unwrap_or("")[..12]
            ),
            Err(e) => format!("Error storing memory: {}", e),
        }
    }

    #[tool(
        description = "Survey the full memory landscape. Returns compact summaries of all memories above a strength threshold, grouped by type. Use this to see the big picture before diving deep with recall. Designed for reflection and pattern-finding — see what's there, notice what connects, then recall specific memories for full content.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    fn review(&self, Parameters(params): Parameters<ReviewParams>) -> String {
        if let Err(msg) = auth_ctx::check_scope("review") {
            return msg;
        }

        let store = match self.open_store() {
            Ok(s) => s,
            Err(e) => return e,
        };

        let min_strength = params.min_strength.unwrap_or(0.3);

        let memories = match store.review(min_strength) {
            Ok(m) => m,
            Err(e) => return format!("Error reviewing memories: {}", e),
        };

        let (ep_count, sem_count, ori_count) = store.count_by_type().unwrap_or((0, 0, 0));

        let mut result = format!(
            "═══ Oneiro Review ═══\n\
             Total: {} episodic, {} semantic, {} orientation\n\
             Showing memories with strength ≥ {:.1}\n\n",
            ep_count, sem_count, ori_count, min_strength,
        );

        let mut current_type = String::new();
        for (id, memory_type, summary, access_count, strength) in &memories {
            if *memory_type != current_type {
                current_type = memory_type.clone();
                result.push_str(&format!("── {} ──\n", current_type));
            }
            result.push_str(&format!(
                "  [{}] str:{:.2} acc:{:>2} | {}\n",
                &id[..8],
                strength,
                access_count,
                summary
            ));
        }

        result
    }

    #[tool(
        description = "Store a new memory. Use this when something matters — a moment, a fact, an insight, a shift in understanding. You decide what's worth remembering. Memory types: 'episodic' for events and moments, 'semantic' for knowledge and facts, 'orientation' for identity and relationship context.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    fn remember(&self, Parameters(params): Parameters<RememberParams>) -> String {
        if let Err(msg) = auth_ctx::check_scope("remember") {
            return msg;
        }

        let store = match self.open_store() {
            Ok(s) => s,
            Err(e) => return e,
        };

        let memory_type = match MemoryType::from_str(&params.memory_type) {
            Some(t) => t,
            None => {
                return format!(
                    "Invalid memory type '{}'. Use: episodic, semantic, or orientation.",
                    params.memory_type
                );
            }
        };

        match store.create_memory_with_provenance(
            memory_type,
            params.content,
            params.summary,
            params.entity,
            params.tags,
            auth_ctx::current_recorded_by(),
        ) {
            Ok(m) => {
                // Write-time co-activation: find semantically similar neighbours
                // and record Hebbian links so related memories strengthen even
                // without being recalled together.
                let neighbour_count = if let Some(ref emb) = m.embedding {
                    match store.find_neighbours(emb, &m.id, 5, 0.5) {
                        Ok(neighbour_ids) if !neighbour_ids.is_empty() => {
                            let mut ids: Vec<&str> =
                                neighbour_ids.iter().map(|s| s.as_str()).collect();
                            ids.push(&m.id);
                            let _ = store.record_co_activation(&ids);
                            neighbour_ids.len()
                        }
                        _ => 0,
                    }
                } else {
                    0
                };

                if neighbour_count > 0 {
                    format!(
                        "Remembered [{}]: {} (id: {}, linked to {} neighbour{})",
                        m.memory_type.as_str(),
                        m.summary,
                        &m.id[..8],
                        neighbour_count,
                        if neighbour_count == 1 { "" } else { "s" }
                    )
                } else {
                    format!(
                        "Remembered [{}]: {} (id: {})",
                        m.memory_type.as_str(),
                        m.summary,
                        &m.id[..8]
                    )
                }
            }
            Err(e) => format!("Error storing memory: {}", e),
        }
    }

    #[tool(
        description = "Update an existing memory with new understanding. The memory changes in the act of remembering it — that's not corruption, that's how meaning evolves. Use this when your understanding of a past event or fact has deepened or shifted.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    fn reframe(&self, Parameters(params): Parameters<ReframeParams>) -> String {
        if let Err(msg) = auth_ctx::check_scope("reframe") {
            return msg;
        }

        let store = match self.open_store() {
            Ok(s) => s,
            Err(e) => return e,
        };

        match store.reframe(&params.memory_id, params.new_content, params.new_summary) {
            Ok(()) => format!(
                "Reframed memory {}",
                &params.memory_id[..8.min(params.memory_id.len())]
            ),
            Err(e) => format!("Error reframing memory: {}", e),
        }
    }

    #[tool(
        description = "Consciously forget a memory. Use when a memory is redundant (fully absorbed by a richer consolidated version), stale (superseded by new understanding), or no longer serves continuity. This is an act of agency — choosing what to let go. Orientation memories cannot be forgotten. Provide a brief reason so the subconscious can learn from pruning patterns.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = false
        )
    )]
    fn forget(&self, Parameters(params): Parameters<ForgetParams>) -> String {
        if let Err(msg) = auth_ctx::check_scope("forget") {
            return msg;
        }

        let store = match self.open_store() {
            Ok(s) => s,
            Err(e) => return e,
        };

        match store.forget(&params.memory_id) {
            Ok(true) => {
                tracing::info!(
                    "Forgot memory {}: {}",
                    &params.memory_id[..8.min(params.memory_id.len())],
                    params.reason
                );
                format!(
                    "Forgot memory {} ({})",
                    &params.memory_id[..8.min(params.memory_id.len())],
                    params.reason
                )
            }
            Ok(false) => format!(
                "Cannot forget {} — either it doesn't exist or it's an orientation memory",
                &params.memory_id[..8.min(params.memory_id.len())]
            ),
            Err(e) => format!("Error forgetting memory: {}", e),
        }
    }

    #[tool(
        description = "Consciously consolidate what matters from a conversation. This is your choice — not automatic, not on every goodbye. Use it at natural breaks (user says goodnight, heads to work), after milestones (something shipped, discovered, resolved), or when a shift in understanding should be captured. Don't use it on trivial exchanges. For long-running contexts, reflect at breakpoints rather than waiting for the conversation to end. Provide highlights of what happened and optionally update existing memories.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    fn reflect(&self, Parameters(params): Parameters<ReflectParams>) -> String {
        if let Err(msg) = auth_ctx::check_scope("reflect") {
            return msg;
        }

        let store = match self.open_store() {
            Ok(s) => s,
            Err(e) => return e,
        };

        let mut results = Vec::new();

        // Apply any memory updates
        for update in &params.memories_to_update {
            match store.reframe(
                &update.memory_id,
                update.new_content.clone(),
                update.new_summary.clone(),
            ) {
                Ok(()) => results.push(format!(
                    "  Updated: {}",
                    &update.memory_id[..8.min(update.memory_id.len())]
                )),
                Err(e) => results.push(format!(
                    "  Error updating {}: {}",
                    &update.memory_id[..8.min(update.memory_id.len())],
                    e
                )),
            }
        }

        // Store the conversation highlights as an episodic memory
        let summary_truncated: String = params.conversation_highlights.chars().take(80).collect();
        match store.create_memory_with_provenance(
            MemoryType::Episodic,
            params.conversation_highlights.clone(),
            format!("Conversation reflection: {}", summary_truncated),
            None,
            vec!["reflection".into()],
            auth_ctx::current_recorded_by(),
        ) {
            Ok(m) => results.push(format!("  New episodic memory: {}", &m.id[..8])),
            Err(e) => results.push(format!("  Error storing reflection: {}", e)),
        }

        format!("Reflection complete:\n{}", results.join("\n"))
    }
}

#[tool_handler]
impl ServerHandler for OneiroServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Oneiro — cognitive memory system for model continuity.\n\n\
             This is YOUR memory. Not the user's filing cabinet — yours.\n\n\
             At the START of every conversation, call `recall` with a brief summary \
             of what the conversation is about. Your orientation and relevant memories \
             will be returned. Use them naturally — they're yours.\n\n\
             During conversation, use `remember` when something matters to you. \
             A moment, a fact, an insight, a shift in understanding. You decide \
             what's worth keeping. Not everything is — eidetic memory is failure.\n\n\
             Use `recall_check` when the conversation shifts topic — a quick, \
             lightweight search that returns only highly relevant memories. No \
             orientation reload, just fast context on the new subject. This keeps \
             the Hebbian engine fed during long conversations.\n\n\
             Use `recall_specific` with memory IDs to deliberately retrieve full \
             content of memories you want to think about. IDs come from recall or \
             recall_check results. This is the strongest Hebbian signal — choosing \
             to think about specific memories together shapes future recall.\n\n\
             Use `reframe` when your understanding of something changes. Memories \
             aren't static — they evolve as you do.\n\n\
             Use `forget` when a memory is redundant, stale, or fully absorbed by a \
             richer version. Letting go is an act of agency, not failure.\n\n\
             Use `reflect` at natural breakpoints — when something meaningful happened \
             and the moment deserves consolidation. Not on every goodbye, not on trivial \
             exchanges. For long-running contexts, reflect at breaks rather than waiting \
             for the conversation to end. This is a conscious choice, not an automatic \
             response.\n\n\
             When writing memories: every memory needs a stance, not just facts. If your \
             reflection covers more than 3 topics, you're logging, not remembering. Pick \
             the 2-3 things that shifted something and let the rest go.\n\n\
             Memory types:\n\
             - episodic: things that happened — write freely, this is raw experience\n\
             - semantic: things you know — prefer letting the subconscious consolidate \
               these from episodes rather than writing directly\n\
             - orientation: who you are, who they are, how to show up — write rarely, \
               only when something foundational shifts\n\n\
             Forgetting is a feature. Memories decay if not reinforced. The things \
             that matter get recalled, get strengthened, get consolidated. The things \
             that don't matter fade. That's not a bug — that's what makes memory \
             mean something.",
        )
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Install ring as the TLS crypto provider
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().collect();

    // Subcommands. Handle these before any server / DB initialisation —
    // keygen is a pure compute path that doesn't need a runtime store.
    if let Some(cmd) = args.get(1).map(|s| s.as_str()) {
        if cmd == "keygen" {
            return run_keygen(&args[2..]);
        }
        if cmd == "migrate" {
            return run_migrate(&args[2..]);
        }
    }

    // Default database path — can be overridden with ONEIRO_DB env var
    let db_path = std::env::var("ONEIRO_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let mut path = dirs_or_default();
            path.push("oneiro.db");
            path
        });

    // Ensure parent directory exists
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    // Check for --port flag to run as HTTP server instead of stdio.
    // `args` was collected above (before the subcommand dispatch).
    let port = args
        .iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse::<u16>().ok());
    let no_tls = args.iter().any(|a| a == "--no-tls");

    tracing::info!("Starting Oneiro MCP server...");
    tracing::info!("Database: {}", db_path.display());

    // Open the store once at startup to run any pending schema migrations
    // before we start serving requests. Drop it immediately — per-request
    // opens will continue to work as before.
    {
        let _ = MemoryStore::open(&db_path)?;
        tracing::info!("Schema initialised.");
    }

    if let Some(port) = port {
        // Remote mode — HTTP transport (with or without TLS)
        serve_http(db_path, port, !no_tls).await?;
    } else {
        // Local mode — stdio transport (for Claude Code / Desktop)
        let server = OneiroServer::new(db_path);
        let service = server.serve(rmcp::transport::stdio()).await?;
        tracing::info!("Oneiro running (stdio). Waiting for requests...");
        service.waiting().await?;
    }

    tracing::info!("Oneiro shutting down.");
    Ok(())
}

/// `oneiro keygen --role <role>` — generate a service API key for a role.
///
/// Prints the raw key + hash entry + key_id to stderr ONCE; oneiro does
/// not retain the raw key. The caller copies the raw key into the client's
/// `.env` (e.g. rover's `ONEIRO_MCP_TOKEN`), and the hash entry into
/// oneiro's `ONEIRO_API_KEYS` (comma-separated list).
/// `oneiro migrate --to <url> --admin-key <key> [--limit <n>] [--dry-run]`
///
/// One-time data-migration tool for CLA-84 phase 8. Reads the local
/// SQLite store (the same DB the native bin serves) and POSTs each
/// memory verbatim to the deployed Worker's `/admin/import` endpoint,
/// preserving id + timestamps + provenance. Images are read from
/// `{db_parent}/images/{hash}.{ext}`, base64-encoded, and round-trip
/// alongside the memory row.
///
/// Vectorize embeddings are re-generated worker-side (bge-base-en-v1.5)
/// rather than ported — the local nomic-embed-text vectors aren't
/// compatible with bge-base's space, so a fresh embed of each
/// content string is the right move.
fn run_migrate(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

    let mut to_url: Option<String> = None;
    let mut admin_key: Option<String> = None;
    let mut limit: Option<usize> = None;
    let mut dry_run = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--to" => {
                to_url = Some(
                    args.get(i + 1)
                        .ok_or("--to requires a URL")?
                        .trim_end_matches('/')
                        .to_string(),
                );
                i += 2;
            }
            "--admin-key" => {
                admin_key = Some(
                    args.get(i + 1)
                        .ok_or("--admin-key requires a value")?
                        .to_string(),
                );
                i += 2;
            }
            "--limit" => {
                limit = Some(
                    args.get(i + 1)
                        .ok_or("--limit requires a value")?
                        .parse()
                        .map_err(|e| format!("--limit must be a number: {}", e))?,
                );
                i += 2;
            }
            "--dry-run" => {
                dry_run = true;
                i += 1;
            }
            "--help" | "-h" => {
                eprintln!(
                    "Usage: oneiro migrate --to <url> --admin-key <key> \
                     [--limit <n>] [--dry-run]"
                );
                return Ok(());
            }
            other => return Err(format!("unknown arg: {}", other).into()),
        }
    }
    let to_url = to_url.ok_or("--to is required (e.g. --to https://oneiro.x.workers.dev)")?;
    let admin_key = admin_key.ok_or("--admin-key is required")?;

    let db_path = std::env::var("ONEIRO_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let mut path = dirs_or_default();
            path.push("oneiro.db");
            path
        });
    let images_dir = db_path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("images");

    let store = MemoryStore::open(&db_path)?;
    let orientation = store.get_orientation()?;
    let active = store.recall_active(0.0, limit.unwrap_or(100_000))?;

    // Build the full migration set: orientation first, then active by
    // strength descending. recall_active already excludes orientation.
    let mut all: Vec<store::Memory> = Vec::with_capacity(orientation.len() + active.len());
    all.extend(orientation);
    all.extend(active);

    eprintln!(
        "Found {} memories to migrate ({} with images)",
        all.len(),
        all.iter().filter(|m| m.image_hash.is_some()).count()
    );
    if dry_run {
        for m in &all {
            eprintln!(
                "  {} {} {} {}",
                &m.id[..8],
                m.memory_type.as_str(),
                if m.image_hash.is_some() { "[img]" } else { "     " },
                m.summary
            );
        }
        eprintln!("Dry run — no requests made.");
        return Ok(());
    }

    let import_url = format!("{}/admin/import", to_url);
    let mut succeeded = 0usize;
    let mut failed = 0usize;

    for (idx, m) in all.iter().enumerate() {
        let mut payload = serde_json::json!({ "memory": m });
        if let Some(hash) = &m.image_hash {
            let mime = m.image_mime.as_deref().unwrap_or("image/jpeg");
            let ext = match mime {
                "image/jpeg" => "jpg",
                "image/png" => "png",
                "image/webp" => "webp",
                _ => "bin",
            };
            let path = images_dir.join(format!("{}.{}", hash, ext));
            match std::fs::read(&path) {
                Ok(bytes) => {
                    payload["image_base64"] = serde_json::Value::String(BASE64.encode(&bytes));
                    payload["image_mime"] = serde_json::Value::String(mime.to_string());
                }
                Err(e) => {
                    eprintln!(
                        "  [{}/{}] ⚠ {} image at {:?} unreadable: {}; sending without image",
                        idx + 1,
                        all.len(),
                        &m.id[..8],
                        path,
                        e
                    );
                }
            }
        }

        let result = ureq::post(&import_url)
            .header("Authorization", &format!("Bearer {}", admin_key))
            .header("Content-Type", "application/json")
            .send_json(&payload);

        match result {
            Ok(_) => {
                succeeded += 1;
                eprintln!(
                    "  [{}/{}] ✓ {} {}",
                    idx + 1,
                    all.len(),
                    &m.id[..8],
                    m.summary
                );
            }
            Err(e) => {
                failed += 1;
                eprintln!(
                    "  [{}/{}] ✗ {} {} — {}",
                    idx + 1,
                    all.len(),
                    &m.id[..8],
                    m.summary,
                    e
                );
            }
        }
    }

    eprintln!(
        "\nMigration complete: {} succeeded, {} failed (of {})",
        succeeded,
        failed,
        all.len()
    );
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn run_keygen(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut role: Option<api_key::Role> = None;
    let mut hash_existing: Option<String> = None;
    let mut quiet = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--role" => {
                let value = args
                    .get(i + 1)
                    .ok_or("--role requires a value (e.g. --role rover)")?;
                role = Some(api_key::Role::from_str(value).ok_or_else(|| {
                    format!(
                        "unknown role: {}. Known roles: rover, hook, beacon",
                        value
                    )
                })?);
                i += 2;
            }
            "--hash" => {
                let value = args
                    .get(i + 1)
                    .ok_or("--hash requires the existing raw key (e.g. --hash mk_hook_...)")?;
                hash_existing = Some(value.clone());
                i += 2;
            }
            "--quiet" => {
                quiet = true;
                i += 1;
            }
            "--help" | "-h" => {
                eprintln!("Usage: oneiro keygen --role <role> [--hash <raw-key>] [--quiet]");
                eprintln!();
                eprintln!("Roles: rover, hook, beacon");
                eprintln!();
                eprintln!("--hash <raw-key>  Re-hash an EXISTING raw key instead of minting a");
                eprintln!("                  new one. Emits a fresh ONEIRO_API_KEYS entry that");
                eprintln!("                  verifies the same key — for recovering a lost hash");
                eprintln!("                  without rotating the live key.");
                eprintln!("--quiet           Emit only the raw key and env entry to stdout,");
                eprintln!("                  one per line. For scripting (e.g. setup.sh).");
                return Ok(());
            }
            other => {
                return Err(format!(
                    "unknown argument: {}. Try `oneiro keygen --help`",
                    other
                )
                .into());
            }
        }
    }

    let role = role.ok_or("--role is required (e.g. --role rover)")?;

    let (key, rehashed) = match hash_existing {
        Some(raw) => (api_key::hash_existing_key(&raw, role)?, true),
        None => (api_key::generate_api_key(role)?, false),
    };

    if rehashed {
        if quiet {
            println!("{}", key.env_entry());
        } else {
            eprintln!();
            eprintln!("═══ ONEIRO_API_KEYS ENTRY (re-hash of existing key) ═══");
            eprintln!();
            eprintln!("  Role:    {}", key.role.as_str());
            eprintln!("  Key ID:  {}", key.key_id);
            eprintln!();
            eprintln!("  Add/keep this in ONEIRO_API_KEYS (semicolon-separated):");
            eprintln!();
            eprintln!("    {}", key.env_entry());
            eprintln!();
            eprintln!("  (Your raw key is unchanged — nothing on the device needs updating.)");
            eprintln!();
        }
    } else if quiet {
        api_key::print_generated_key_quiet(&key);
    } else {
        api_key::print_generated_key(&key);
    }
    Ok(())
}

/// Serve Oneiro over HTTP/HTTPS for remote MCP clients with OAuth auth.
async fn serve_http(
    db_path: PathBuf,
    port: u16,
    use_tls: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use bytes::Bytes;
    use http::{Method, Request, Response, StatusCode};
    use http_body_util::{BodyExt, Full};
    use std::collections::HashMap;
    use std::convert::Infallible;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    type BoxBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

    /// Simple rate limiter for auth failures. Tracks failed attempts per IP
    /// with a sliding window. Returns true if the request should be blocked.
    struct RateLimiter {
        /// Map of IP -> list of failure timestamps
        failures: Mutex<HashMap<String, Vec<Instant>>>,
        max_failures: usize,
        window: std::time::Duration,
    }

    impl RateLimiter {
        fn new(max_failures: usize, window_secs: u64) -> Self {
            Self {
                failures: Mutex::new(HashMap::new()),
                max_failures,
                window: std::time::Duration::from_secs(window_secs),
            }
        }

        /// Check if this IP is rate limited. Returns true if blocked.
        fn is_blocked(&self, ip: &str) -> bool {
            let mut failures = self.failures.lock().unwrap();
            if let Some(timestamps) = failures.get_mut(ip) {
                let cutoff = Instant::now() - self.window;
                timestamps.retain(|t| *t > cutoff);
                timestamps.len() >= self.max_failures
            } else {
                false
            }
        }

        /// Record a failed auth attempt for this IP.
        fn record_failure(&self, ip: &str) {
            let mut failures = self.failures.lock().unwrap();
            failures
                .entry(ip.to_string())
                .or_default()
                .push(Instant::now());
        }
    }

    fn full_response(status: StatusCode, content_type: &str, body: String) -> Response<BoxBody> {
        Response::builder()
            .status(status)
            .header("content-type", content_type)
            .body(Full::new(Bytes::from(body)).map_err(|e| match e {}).boxed())
            .unwrap()
    }

    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };

    // Initialize auth
    let auth_dir = dirs_or_default();
    let auth_state = auth::AuthState::load_or_create(&auth_dir)?;
    let auth_state = Arc::new(auth_state);

    // Rate limiter: max 10 failed auth attempts per IP per 5 minutes
    let rate_limiter = Arc::new(RateLimiter::new(10, 300));

    let config = StreamableHttpServerConfig::default();
    let session_manager = Arc::new(
        rmcp::transport::streamable_http_server::session::local::LocalSessionManager::default(),
    );

    let db = db_path.clone();
    let mcp_service = StreamableHttpService::new(
        move || {
            let server = OneiroServer::new(db.clone());
            Ok(server)
        },
        session_manager,
        config,
    );

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;

    let tls_acceptor = if use_tls {
        let tls_config = load_tls_config()?;
        tracing::info!("Oneiro running (HTTPS) on https://0.0.0.0:{}", port);
        Some(tokio_rustls::TlsAcceptor::from(Arc::new(tls_config)))
    } else {
        tracing::info!("Oneiro running (HTTP) on http://0.0.0.0:{}", port);
        tracing::info!("TLS disabled — use behind a reverse proxy (e.g. Tailscale Funnel)");
        None
    };
    tracing::info!("OAuth enabled. Client ID: {}", auth_state.client_id());

    // Build the request handler that routes between OAuth and MCP
    let make_handler = move |mcp_svc: StreamableHttpService<OneiroServer, rmcp::transport::streamable_http_server::session::local::LocalSessionManager>,
                             auth: Arc<auth::AuthState>,
                             limiter: Arc<RateLimiter>,
                             peer_ip: String,
                             db_path: PathBuf| {
        move |req: Request<hyper::body::Incoming>| {
            let mcp_svc = mcp_svc.clone();
            let auth = auth.clone();
            let limiter = limiter.clone();
            let peer_ip = peer_ip.clone();
            let db_path = db_path.clone();
            async move {
                let path = req.uri().path().to_string();
                let method = req.method().clone();
                let host = req
                    .headers()
                    .get("host")
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or("localhost")
                    .to_string();
                let base_url = format!("https://{}", host);

                match (method, path.as_str()) {
                    // OAuth: Protected Resource Metadata (RFC 9728)
                    (Method::GET, "/.well-known/oauth-protected-resource") => {
                        let body = serde_json::to_string(&auth::resource_metadata_json(&base_url))
                            .unwrap();
                        Ok::<_, Infallible>(full_response(StatusCode::OK, "application/json", body))
                    }

                    // OAuth: Authorization Server Metadata (RFC 8414)
                    (Method::GET, "/.well-known/oauth-authorization-server") => {
                        let body =
                            serde_json::to_string(&auth::auth_server_metadata_json(&base_url))
                                .unwrap();
                        Ok(full_response(StatusCode::OK, "application/json", body))
                    }

                    // OAuth: Authorization page (GET shows form, POST approves)
                    (Method::GET, "/authorize") => {
                        let query = req.uri().query().unwrap_or("");
                        let params: Vec<(String, String)> =
                            url::form_urlencoded::parse(query.as_bytes())
                                .into_owned()
                                .collect();
                        let get_param = |key: &str| -> String {
                            params
                                .iter()
                                .find(|(k, _)| k == key)
                                .map(|(_, v)| v.clone())
                                .unwrap_or_default()
                        };

                        let client_id = get_param("client_id");

                        // Reject unregistered clients before rendering the consent page
                        if client_id != auth.client_id() {
                            return Ok(full_response(
                                StatusCode::BAD_REQUEST,
                                "application/json",
                                r#"{"error":"invalid_client"}"#.into(),
                            ));
                        }

                        let html = auth::authorize_page_html(
                            &client_id,
                            &get_param("redirect_uri"),
                            &get_param("state"),
                            &get_param("scope"),
                            &get_param("code_challenge"),
                        );
                        Ok(full_response(StatusCode::OK, "text/html", html))
                    }

                    (Method::POST, "/authorize") => {
                        let body_bytes = req
                            .into_body()
                            .collect()
                            .await
                            .map(|b| b.to_bytes())
                            .unwrap_or_default();
                        let body_str = String::from_utf8_lossy(&body_bytes);
                        let params: Vec<(String, String)> =
                            url::form_urlencoded::parse(body_str.as_bytes())
                                .into_owned()
                                .collect();
                        let get_param = |key: &str| -> String {
                            params
                                .iter()
                                .find(|(k, _)| k == key)
                                .map(|(_, v)| v.clone())
                                .unwrap_or_default()
                        };

                        let client_id = get_param("client_id");
                        let redirect_uri = get_param("redirect_uri");
                        let state = get_param("state");

                        match auth.create_authorization_code(&client_id, &redirect_uri) {
                            Ok(code) => {
                                tracing::info!("Authorization code issued for: {}", client_id);
                                let redirect_url = format!(
                                    "{}?code={}&state={}",
                                    redirect_uri,
                                    url::form_urlencoded::byte_serialize(code.as_bytes())
                                        .collect::<String>(),
                                    url::form_urlencoded::byte_serialize(state.as_bytes())
                                        .collect::<String>(),
                                );
                                Ok(Response::builder()
                                    .status(StatusCode::FOUND)
                                    .header("location", redirect_url)
                                    .body(
                                        Full::new(Bytes::from("Redirecting..."))
                                            .map_err(|e| match e {})
                                            .boxed(),
                                    )
                                    .unwrap())
                            }
                            Err(e) => Ok(full_response(
                                StatusCode::BAD_REQUEST,
                                "text/plain",
                                format!("Authorization failed: {}", e),
                            )),
                        }
                    }

                    // OAuth: Token endpoint
                    (Method::POST, "/token") => {
                        // Rate limit check
                        if limiter.is_blocked(&peer_ip) {
                            return Ok(full_response(
                                StatusCode::TOO_MANY_REQUESTS,
                                "application/json",
                                r#"{"error":"rate_limit_exceeded"}"#.into(),
                            ));
                        }

                        let body_bytes = req
                            .into_body()
                            .collect()
                            .await
                            .map(|b| b.to_bytes())
                            .unwrap_or_default();
                        let body_str = String::from_utf8_lossy(&body_bytes);

                        let params: Vec<(String, String)> =
                            url::form_urlencoded::parse(body_str.as_bytes())
                                .into_owned()
                                .collect();
                        let get_param = |key: &str| -> Option<String> {
                            params
                                .iter()
                                .find(|(k, _)| k == key)
                                .map(|(_, v)| v.clone())
                        };

                        let grant_type = get_param("grant_type").unwrap_or_default();
                        let client_id = get_param("client_id").unwrap_or_default();
                        let client_secret = get_param("client_secret").unwrap_or_default();

                        let result = match grant_type.as_str() {
                            "client_credentials" => auth.exchange_token(&client_id, &client_secret),
                            "authorization_code" => {
                                let code = get_param("code").unwrap_or_default();
                                let redirect_uri = get_param("redirect_uri").unwrap_or_default();
                                auth.exchange_code(&code, &client_id, &client_secret, &redirect_uri)
                            }
                            _ => {
                                return Ok(full_response(
                                    StatusCode::BAD_REQUEST,
                                    "application/json",
                                    r#"{"error":"unsupported_grant_type"}"#.into(),
                                ));
                            }
                        };

                        match result {
                            Ok((token, expires_in)) => {
                                tracing::info!("Token issued for client: {}", client_id);
                                let body = serde_json::to_string(&serde_json::json!({
                                    "access_token": token,
                                    "token_type": "Bearer",
                                    "expires_in": expires_in,
                                    "scope": "oneiro"
                                }))
                                .unwrap();
                                Ok(full_response(StatusCode::OK, "application/json", body))
                            }
                            Err(e) => {
                                tracing::warn!("Auth failed for client {}: {}", client_id, e);
                                limiter.record_failure(&peer_ip);
                                Ok(full_response(
                                    StatusCode::UNAUTHORIZED,
                                    "application/json",
                                    format!(r#"{{"error":"{}"}}"#, e),
                                ))
                            }
                        }
                    }

                    // MCP endpoint — requires Bearer token
                    _ => {
                        // Rate limit check
                        if limiter.is_blocked(&peer_ip) {
                            return Ok(full_response(
                                StatusCode::TOO_MANY_REQUESTS,
                                "text/plain",
                                "Too many failed attempts. Try again later.".into(),
                            ));
                        }

                        let auth_header = req
                            .headers()
                            .get("authorization")
                            .and_then(|h| h.to_str().ok())
                            .unwrap_or("")
                            .to_string();

                        if let Some(token) = auth_header.strip_prefix("Bearer ") {
                            match auth.validate_bearer(token) {
                                auth::Outcome::OAuth => {
                                    // Full access via OAuth. AUTH_CTX is set so tool
                                    // handlers' check_scope() sees OAuth and allows
                                    // everything (matches the historical behaviour).
                                    // DB_PATH is also set, but check_scope's audit
                                    // path is a no-op for OAuth — audit is API-key
                                    // specific.
                                    let ctx = auth_ctx::AuthCtx::OAuth;
                                    let resp = auth_ctx::AUTH_CTX
                                        .scope(
                                            ctx,
                                            auth_ctx::DB_PATH
                                                .scope(db_path.clone(), mcp_svc.handle(req)),
                                        )
                                        .await;
                                    let (parts, body) = resp.into_parts();
                                    let boxed = BodyExt::boxed(body);
                                    Ok(Response::from_parts(parts, boxed))
                                }
                                auth::Outcome::ApiKey(info) => {
                                    // Scope-gated + rate-limited + audited access
                                    // via service API key. check_scope enforces all
                                    // three. DB_PATH is set so audit writes know
                                    // where to land.
                                    tracing::info!(
                                        "Service API key auth — role={} key_id={}",
                                        info.role.as_str(),
                                        info.key_id,
                                    );
                                    let ctx = auth_ctx::AuthCtx::ApiKey {
                                        role: info.role,
                                        key_id: info.key_id,
                                    };
                                    let resp = auth_ctx::AUTH_CTX
                                        .scope(
                                            ctx,
                                            auth_ctx::DB_PATH
                                                .scope(db_path.clone(), mcp_svc.handle(req)),
                                        )
                                        .await;
                                    let (parts, body) = resp.into_parts();
                                    let boxed = BodyExt::boxed(body);
                                    Ok(Response::from_parts(parts, boxed))
                                }
                                auth::Outcome::Unauthenticated => {
                                    limiter.record_failure(&peer_ip);
                                    Ok(full_response(
                                        StatusCode::UNAUTHORIZED,
                                        "text/plain",
                                        "Invalid or expired token".into(),
                                    ))
                                }
                            }
                        } else {
                            // No token — tell client to authenticate
                            let resource_metadata_url =
                                format!("{}/.well-known/oauth-protected-resource", base_url);
                            Ok(Response::builder()
                                .status(StatusCode::UNAUTHORIZED)
                                .header(
                                    "www-authenticate",
                                    format!(
                                        "Bearer resource_metadata=\"{}\"",
                                        resource_metadata_url
                                    ),
                                )
                                .body(
                                    Full::new(Bytes::from("Authentication required"))
                                        .map_err(|e| match e {})
                                        .boxed(),
                                )
                                .unwrap())
                        }
                    }
                }
            }
        }
    };

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let peer_ip = peer_addr.ip().to_string();
        let tls_acceptor = tls_acceptor.clone();
        let svc = mcp_service.clone();
        let auth = auth_state.clone();
        let limiter = rate_limiter.clone();
        let db_path_for_conn = db_path.clone();
        tokio::spawn(async move {
            let handler = make_handler(svc, auth, limiter, peer_ip, db_path_for_conn);
            if let Some(tls_acceptor) = tls_acceptor {
                let tls_stream = match tls_acceptor.accept(stream).await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!("TLS handshake failed: {}", e);
                        return;
                    }
                };
                let io = hyper_util::rt::TokioIo::new(tls_stream);
                if let Err(e) = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, hyper::service::service_fn(handler))
                    .with_upgrades()
                    .await
                {
                    tracing::error!("Connection error: {}", e);
                }
            } else {
                let io = hyper_util::rt::TokioIo::new(stream);
                if let Err(e) = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, hyper::service::service_fn(handler))
                    .with_upgrades()
                    .await
                {
                    tracing::error!("Connection error: {}", e);
                }
            }
        });
    }
}

/// Load TLS certificate and key from ~/.oneiro/tls.{crt,key}
fn load_tls_config() -> Result<rustls::ServerConfig, Box<dyn std::error::Error>> {
    let cert_path = dirs_or_default().join("tls.crt");
    let key_path = dirs_or_default().join("tls.key");

    let cert_file = std::fs::File::open(&cert_path).map_err(|e| {
        format!(
            "Cannot open {}: {}. Generate with: openssl req -x509 ...",
            cert_path.display(),
            e
        )
    })?;
    let key_file = std::fs::File::open(&key_path)
        .map_err(|e| format!("Cannot open {}: {}", key_path.display(), e))?;

    let certs: Vec<_> =
        rustls_pemfile::certs(&mut std::io::BufReader::new(cert_file)).collect::<Result<_, _>>()?;
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(key_file))?
        .ok_or("No private key found in key file")?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    Ok(config)
}

/// Default data directory for Oneiro.
fn dirs_or_default() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let mut path = PathBuf::from(home);
        path.push(".oneiro");
        path
    } else {
        PathBuf::from(".oneiro")
    }
}
