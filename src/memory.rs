// memory.rs — Universal type definitions for oneiro.
//
// Lives outside store.rs so the wasm32 worker side (worker_store.rs) and
// the native rusqlite side (store.rs) can share the same `Memory` and
// `MemoryType` types. No DB-specific dependencies live here — only
// chrono + serde, both wasm32-compatible.

use chrono::{DateTime, Utc};

/// The three types of memory, flowing upward:
/// Episodes → consolidate → Semantics → distil → Orientation
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MemoryType {
    /// Things that happened. Subject to decay. Surfaced by association.
    Episodic,
    /// Things I know. Distilled from episodes. More stable.
    Semantic,
    /// Who am I, who are you, what are we. Always loaded. Core of continuity.
    Orientation,
}

impl MemoryType {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryType::Episodic => "episodic",
            MemoryType::Semantic => "semantic",
            MemoryType::Orientation => "orientation",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "episodic" => Some(MemoryType::Episodic),
            "semantic" => Some(MemoryType::Semantic),
            "orientation" => Some(MemoryType::Orientation),
            _ => None,
        }
    }

    /// Base stability for new memories of this type.
    /// Higher = decays slower. Orientation is effectively permanent.
    ///
    /// Episodic stability was raised from 1.0 to 7.0 in CLA-87 to counter
    /// the decay-asymmetry problem: with stability=1.0 a new memory not
    /// retrieved by semantic match in its first week dies before it had
    /// a chance to be load-bearing in a future context. At stability=7.0
    /// the same memory decays to ~0.37 at 7 days and ~0.014 at 30 days,
    /// widening the gauntlet from a week to a month — long enough for
    /// the conversation that would have naturally surfaced it to happen.
    pub fn base_stability(&self) -> f64 {
        match self {
            MemoryType::Episodic => 7.0,      // decays in ~30 days without reinforcement
            MemoryType::Semantic => 7.0,      // decays in weeks
            MemoryType::Orientation => 365.0, // effectively permanent
        }
    }
}

/// A single memory in the store.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Memory {
    /// Unique identifier
    pub id: String,
    /// What type of memory this is
    pub memory_type: MemoryType,
    /// The content of the memory
    pub content: String,
    /// Brief summary for retrieval context (what the memory is about)
    pub summary: String,
    /// When this memory was created
    pub created_at: DateTime<Utc>,
    /// When this memory was last accessed/surfaced
    pub last_accessed: DateTime<Utc>,
    /// Number of times this memory has been recalled
    pub access_count: u32,
    /// Current recall strength (0.0 = forgotten, 1.0 = vivid)
    /// Computed from Ebbinghaus: strength = e^(-time_since_access / stability)
    pub strength: f64,
    /// Stability factor — increases with each recall, slowing future decay
    pub stability: f64,
    /// Optional: which entity/relationship this memory relates to
    pub entity: Option<String>,
    /// Optional: tags for association
    pub tags: Vec<String>,
    /// Embedding vector for semantic search (None if not yet embedded).
    /// On native this is loaded from the SQLite blob column; on wasm32
    /// (post-CLA-84) the vector lives in Vectorize and this field is
    /// typically None on Memory instances flowing through the worker.
    /// Skipped in serde because it's huge and reconstructable.
    #[serde(skip)]
    pub embedding: Option<Vec<f64>>,
    /// Optional: SHA-256 hex hash of the associated image file.
    /// On native the bytes live at {images_dir}/{hash}.{ext} (content-
    /// addressed storage); on the worker they live in R2 under the same
    /// key shape. Serialized so the migration path (CLA-84 phase 8) can
    /// round-trip image metadata.
    #[serde(default)]
    pub image_hash: Option<String>,
    /// Optional: MIME type of the image (e.g. "image/jpeg"). Determines
    /// file extension. Same serialization rationale as `image_hash`.
    #[serde(default)]
    pub image_mime: Option<String>,
    /// Optional: provenance — who or what recorded this memory.
    /// Server-controlled (set from auth context); any client-supplied value
    /// is ignored to prevent forgery.
    ///   "claude" — written by a Claude instance via OAuth
    ///   "rover"  — written by the rover via its service API key
    ///   None     — pre-CLA-86 legacy memory, or a local stdio write
    pub recorded_by: Option<String>,
}

impl Memory {
    /// True if this memory satisfies every active recall_check filter
    /// (CLA-108). Each argument is independently optional / empty:
    ///
    /// * `entity` — if `Some`, the memory's entity must equal it exactly
    ///   (case-sensitive). `None` skips the entity check.
    /// * `memory_type` — if `Some`, the memory's type must equal it.
    /// * `tags` — if non-empty, at least one of the listed tags must
    ///   appear in the memory's tag list (any-of, not all-of).
    ///
    /// The empty filter (all None / empty) passes every memory — so the
    /// no-filter path through recall_check is numerically identical to
    /// pre-CLA-108 behaviour. Filters compose with AND semantics across
    /// fields, OR semantics within the tag list.
    pub fn matches_filter(
        &self,
        entity: Option<&str>,
        memory_type: Option<MemoryType>,
        tags: &[String],
    ) -> bool {
        if let Some(want) = entity {
            if self.entity.as_deref() != Some(want) {
                return false;
            }
        }
        if let Some(want) = memory_type {
            if self.memory_type != want {
                return false;
            }
        }
        if !tags.is_empty() {
            let any_match = tags
                .iter()
                .any(|wanted| self.tags.iter().any(|have| have == wanted));
            if !any_match {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(entity: Option<&str>, memory_type: MemoryType, tags: &[&str]) -> Memory {
        Memory {
            id: "test".to_string(),
            memory_type,
            content: String::new(),
            summary: String::new(),
            created_at: Utc::now(),
            last_accessed: Utc::now(),
            access_count: 0,
            strength: 1.0,
            stability: 1.0,
            entity: entity.map(str::to_string),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            embedding: None,
            image_hash: None,
            image_mime: None,
            recorded_by: None,
        }
    }

    #[test]
    fn empty_filter_passes_everything() {
        let m = mem(Some("chopper"), MemoryType::Episodic, &["walk", "morning"]);
        assert!(m.matches_filter(None, None, &[]));
    }

    #[test]
    fn entity_filter_exact_match() {
        let m = mem(Some("chopper"), MemoryType::Episodic, &[]);
        assert!(m.matches_filter(Some("chopper"), None, &[]));
        assert!(!m.matches_filter(Some("rover"), None, &[]));
    }

    #[test]
    fn entity_filter_rejects_when_memory_entity_is_none() {
        let m = mem(None, MemoryType::Episodic, &[]);
        assert!(!m.matches_filter(Some("chopper"), None, &[]));
    }

    #[test]
    fn memory_type_filter() {
        let m = mem(None, MemoryType::Semantic, &[]);
        assert!(m.matches_filter(None, Some(MemoryType::Semantic), &[]));
        assert!(!m.matches_filter(None, Some(MemoryType::Episodic), &[]));
    }

    #[test]
    fn tags_any_of() {
        let m = mem(None, MemoryType::Episodic, &["walk", "morning"]);
        assert!(m.matches_filter(None, None, &["walk".to_string()]));
        assert!(m.matches_filter(None, None, &["walk".to_string(), "absent".to_string()]));
        assert!(!m.matches_filter(None, None, &["evening".to_string()]));
    }

    #[test]
    fn tags_empty_skip_check() {
        // Empty tags arg means "don't filter by tags" — even a memory with
        // no tags should pass.
        let m = mem(None, MemoryType::Episodic, &[]);
        assert!(m.matches_filter(None, None, &[]));
    }

    #[test]
    fn filters_compose_with_and_semantics() {
        let m = mem(Some("chopper"), MemoryType::Episodic, &["walk"]);
        assert!(m.matches_filter(
            Some("chopper"),
            Some(MemoryType::Episodic),
            &["walk".to_string()]
        ));
        // One mismatch breaks the conjunction.
        assert!(!m.matches_filter(
            Some("chopper"),
            Some(MemoryType::Semantic),
            &["walk".to_string()]
        ));
    }
}
