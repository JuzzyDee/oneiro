// worker_orient.rs — Orientation-first conversation entry-point (CLA-103).
//
// Provides the "fast, deterministic, no-semantic-search" payload that
// `recall_orient` (MCP tool) and `GET /orientation` (HTTP endpoint)
// both produce: all orientation memories, optionally followed by N
// most recent episodics. No embedding call, no Vectorize query, no
// MMR rerank — the work that makes this payload good already happened
// upstream during reflect/dialectic passes; here we just surface it.
//
// Two consumers:
//   - `worker_mcp::handle_recall_orient` — returns orientation + recent
//     episodics for the model when it consciously calls the tool.
//   - `lib.rs` GET /orientation — returns orientation only (no
//     episodics) as plain text for the SessionStart/PreCompact hook
//     to inject. The hook's job is identity grounding before any
//     tool evaluation fires; "what happened lately" is the model's
//     to ask for deliberately via the MCP tool.
//
// Why the split: a SessionStart hook can fire many times per day
// (every new conversation + every PreCompact). Surfacing recent
// episodics there would inflate their access counts artificially and
// pollute the Hebbian co-activation signal with automatic surfacing
// noise. Orientation memories are pinned at strength 1.0 anyway, so
// re-rendering them on every hook fire is a no-op for the decay model.

use crate::memory::Memory;
use chrono::Utc;

/// Render the standard orientation payload.
///
/// `orientation` is always rendered when non-empty. `recent` is
/// optional — `Some(&[])` and `None` render the same way (no Recent
/// section), but the type distinction lets callers signal intent at
/// the call site (`/orientation` always passes `None`; `recall_orient`
/// always passes `Some(recent)` even when empty).
///
/// `counts` is `(episodic, semantic, orientation)` totals, used for
/// the header line that gives the model a sense of the store's shape
/// without having to call `review`.
pub fn format_payload(
    orientation: &[Memory],
    recent: Option<&[Memory]>,
    counts: (u64, u64, u64),
) -> String {
    let (ep, sem, ori) = counts;
    let mut out = format!(
        "═══ Oneiro ═══\nMemory store: {} episodic, {} semantic, {} orientation\n\n",
        ep, sem, ori
    );

    if !orientation.is_empty() {
        out.push_str("── Orientation (always present) ──\n");
        for m in orientation {
            out.push_str(&format_memory(m));
        }
        out.push('\n');
    }

    if let Some(recent_memories) = recent {
        if !recent_memories.is_empty() {
            out.push_str("── Recent ──\n");
            for m in recent_memories {
                out.push_str(&format_memory_brief(m));
            }
        }
    }

    if orientation.is_empty() && recent.map_or(true, |r| r.is_empty()) {
        out.push_str("No memories yet. This is a fresh start.\n");
    }

    out
}

/// The `[type | age | str | entity | id:prefix [tags] via:source img]` header
/// line, shared by the full and brief renderers below so they can never drift.
/// The 8-char id prefix is what callers reference in `recall_specific`,
/// `reframe`, `forget` — short enough to retype, long enough to be unambiguous.
fn format_header(m: &Memory) -> String {
    let type_label = m.memory_type.as_str();
    let entity_str = m.entity.as_deref().unwrap_or("");
    let tags_str = if m.tags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", m.tags.join(", "))
    };
    let age = Utc::now() - m.created_at;
    let age_str = if age.num_days() > 0 {
        format!("{}d ago", age.num_days())
    } else if age.num_hours() > 0 {
        format!("{}h ago", age.num_hours())
    } else {
        "just now".to_string()
    };
    let by = m
        .recorded_by
        .as_deref()
        .map(|s| format!(" via:{}", s))
        .unwrap_or_default();
    // Image indicator — terse so it doesn't bloat the header. The reader
    // sees `img` and knows recall_image will succeed against this id.
    let img = if m.image_hash.is_some() { " | img" } else { "" };
    format!(
        "[{} | {} | str:{:.2} | {} | id:{}{}{}{}]",
        type_label, age_str, m.strength, entity_str, &m.id[..8], tags_str, by, img
    )
}

/// Header + full `content`. For orientation (the always-loaded axes — you want
/// their whole text), and anywhere a complete memory should render. Shared with
/// `worker_mcp` so the same memory looks the same wherever it surfaces.
pub(crate) fn format_memory(m: &Memory) -> String {
    format!("{}\n{}\n", format_header(m), m.content)
}

/// Header + one-line `summary` only. For the "Recent" surface, which shows the
/// distilled semantics of the last capture and must stay light — full content
/// there would re-bloat the exact thing recall_orient exists to keep small.
pub(crate) fn format_memory_brief(m: &Memory) -> String {
    format!("{}\n{}\n", format_header(m), m.summary)
}
