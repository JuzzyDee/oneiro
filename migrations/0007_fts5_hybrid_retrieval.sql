-- migrations/0007_fts5_hybrid_retrieval.sql — Hybrid retrieval (CLA-109).
--
-- BM25-ranked full-text search via SQLite's FTS5 virtual table, queried
-- in parallel with Vectorize at recall time and fused with Reciprocal
-- Rank Fusion. Lexical search catches exact-term hits cosine misses
-- (names, jargon, unique phrases). Semantic search catches conceptual
-- hits lexical misses. Together substantially better than either alone.
--
-- Standalone (not external-content) FTS5 table — keeps the index self-
-- contained at the cost of ~2x storage for the indexed columns. At our
-- scale (~500 memories × ~500 bytes each = ~250KB extra) the tradeoff
-- favours simplicity. External-content would couple the index to the
-- memories table's rowid, which is implicit and brittle to schema work.
--
-- Triggers keep memories_fts in sync with memories. The backfill INSERT
-- at the end seeds the index with rows that pre-date this migration.
-- Direct INSERTs into memories_fts (like the backfill) don't fire the
-- memories_fts_insert trigger, so the backfill is safe and won't double-
-- index. The migration framework guarantees one-shot apply.

CREATE VIRTUAL TABLE memories_fts USING fts5(
    id UNINDEXED,        -- carried for SELECT, not tokenised for MATCH
    content,
    summary,
    entity,              -- empty string when memories.entity IS NULL
    tags,                -- JSON-stringified; unicode61 tokeniser handles
                         -- the brackets/quotes fine — splits on those
                         -- non-alphanumeric chars as boundaries.
    tokenize='unicode61 remove_diacritics 1'
);

-- AFTER triggers fire post-mutation, so memories_fts always sees the
-- committed state. COALESCE on entity/tags handles the NULL columns
-- (FTS5 stores empty strings for missing token fields).
CREATE TRIGGER memories_fts_insert AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(id, content, summary, entity, tags)
    VALUES (new.id, new.content, new.summary,
            COALESCE(new.entity, ''), COALESCE(new.tags, '[]'));
END;

CREATE TRIGGER memories_fts_update AFTER UPDATE ON memories BEGIN
    UPDATE memories_fts SET
        content = new.content,
        summary = new.summary,
        entity  = COALESCE(new.entity, ''),
        tags    = COALESCE(new.tags, '[]')
    WHERE id = new.id;
END;

CREATE TRIGGER memories_fts_delete AFTER DELETE ON memories BEGIN
    DELETE FROM memories_fts WHERE id = old.id;
END;

-- Backfill: index every existing row. Pre-CLA-109 deploys have memories
-- without a corresponding FTS5 entry; this catches them up.
INSERT INTO memories_fts(id, content, summary, entity, tags)
SELECT id, content, summary, COALESCE(entity, ''), COALESCE(tags, '[]')
FROM memories;
