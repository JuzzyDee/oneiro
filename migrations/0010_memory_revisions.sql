-- migrations/0010_memory_revisions.sql — make reframe non-destructive (CLA-130).
--
-- Background: `worker_store::reframe` overwrites a memory's content/summary
-- in place. It is the ONLY destructive operation in the store. The dialectic
-- already protects its own reframes via `memory_reframes` (0006), but the
-- encode judge and the orient distiller — and the conscious reframe/reflect
-- MCP tools — called the bare primitive, so a mis-targeted revise destroyed
-- the original with no undo. (This is exactly what paused the capture queue:
-- the encode judge confused itself via hybrid-search similarity and wrote
-- over three unrelated semantics. No attacker — a write primitive over the
-- whole store, plus a confusable target step.)
--
-- The fix binds the undo to the primitive itself: `reframe` now commits an
-- atomic D1 batch — UPDATE memories + INSERT memory_revisions — so the
-- overwrite can never land without the original being preserved first.
-- Every caller (encode, orient, reframe-tool, reflect-tool) is reversible
-- by construction; no caller has to remember to be careful.
--
-- This table is the generalised sibling of memory_reframes: same safety
-- role, but caller-agnostic (a `source` discriminator instead of a
-- dialectic-decision FK) so it serves every non-dialectic reframe. No
-- foreign key, deliberately — memory_id may point at a row that is later
-- forgotten, and we do not want an audit insert to mask or block that.
--
-- Query patterns this enables:
--
--   -- What did a given subsystem rewrite recently? (forensics after a mis-fire)
--   SELECT revised_at, memory_id, old_summary, new_summary, rationale
--   FROM memory_revisions
--   WHERE source = 'encode'
--   ORDER BY revised_at DESC LIMIT 20;
--
--   -- Full rewrite history of one memory (rollback context)
--   SELECT revised_at, source, old_content, new_content
--   FROM memory_revisions
--   WHERE memory_id = ?
--   ORDER BY revised_at DESC;

CREATE TABLE IF NOT EXISTS memory_revisions (
    revision_id  TEXT PRIMARY KEY,
    memory_id    TEXT NOT NULL,
    -- which caller drove this reframe: 'encode' | 'orient' | 'reframe-tool' | 'reflect-tool'
    source       TEXT NOT NULL,
    -- the judge's stated reason (the relationship gate). Empty for the
    -- conscious MCP paths, which carry no judge rationale.
    rationale    TEXT NOT NULL DEFAULT '',
    old_content  TEXT NOT NULL,
    old_summary  TEXT NOT NULL,
    new_content  TEXT NOT NULL,
    new_summary  TEXT NOT NULL,
    revised_at   TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_memory_revisions_memory
    ON memory_revisions(memory_id);
CREATE INDEX IF NOT EXISTS idx_memory_revisions_revised_at
    ON memory_revisions(revised_at DESC);
CREATE INDEX IF NOT EXISTS idx_memory_revisions_source
    ON memory_revisions(source, revised_at DESC);
