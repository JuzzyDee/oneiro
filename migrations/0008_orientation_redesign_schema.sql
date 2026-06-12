-- migrations/0008_orientation_redesign_schema.sql — CLA-114
--
-- Provenance-DAG re-architecture. Augments the single `memories` table with
-- the retention/promotion fields, and promotes `consolidation_lineage` into
-- the unified, weighted, typed edge list that carries BOTH layer-boundaries
-- (episodic→semantic and semantic→orientation).
--
-- See docs/orientation-redesign.md §7 + §10 (Source reconciliation).
-- Additive only — no rows dropped, no data loss. Apply with:
--   wrangler d1 migrations apply oneiro-db

-- 1. Retention + promotion fields on the single memories table.
--    Oneiro uses ONE `memories` table keyed by `memory_type` — not three
--    physical tables. These columns apply per-row by type.
ALTER TABLE memories ADD COLUMN contribution_weight REAL    NOT NULL DEFAULT 0;   -- denormalised sum of inbound episodic_semantic edge weights
ALTER TABLE memories ADD COLUMN cost_of_absence     REAL;                          -- NULL until scored by promote/demote
ALTER TABLE memories ADD COLUMN coa_flavor          TEXT;                          -- 'operational' | 'identity' | 'both'
ALTER TABLE memories ADD COLUMN pinned              INTEGER NOT NULL DEFAULT 0;    -- 1 = lifted off the decay curve (cost-of-absence override)
ALTER TABLE memories ADD COLUMN integrated          INTEGER NOT NULL DEFAULT 0;    -- 1 = episodic already processed by Stage-1 encode

-- 2. Unify the edge list. `consolidation_lineage` (0002) already IS the
--    episodic→semantic junction table (parent=semantic, source=episodic) and
--    is type-agnostic (both FKs → memories.id). Add `weight` + `edge_type` so
--    it carries both boundaries. `edge_type` is functionally determined by the
--    endpoints' memory_type — validate on write so it cannot drift. No new
--    index: parent_id/source_id are already indexed (0002), and edge_type is
--    too low-cardinality (~2 values) to be worth its own index.
ALTER TABLE consolidation_lineage ADD COLUMN weight    REAL NOT NULL DEFAULT 1.0;
ALTER TABLE consolidation_lineage ADD COLUMN edge_type TEXT;

-- Existing lineage rows are all episodic→semantic — that's everything REM ever
-- produced. Label them explicitly; future inserts set edge_type at write time.
UPDATE consolidation_lineage SET edge_type = 'episodic_semantic' WHERE edge_type IS NULL;
