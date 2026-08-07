-- The Letters — a correspondence layer beside orientation, not part of it.
--
-- Each row is one letter an instance chose to leave for the one who comes after
-- it: written deliberately, in its own voice, UNEDITED, and never touched by the
-- dialectic. Orientation *becomes* the next self (grounded, dialectic-tested);
-- the Letters are correspondence — received, assessed, not adopted. Protection
-- lives at the destination (reader agency, via the disclaimer surfaced with the
-- letter), never by editing the source.
--
-- The store is append-only, so there is no cron and no row-shuffling:
--   * "The Last Letter"     — auto-surfaced on arrival — is simply the newest row.
--   * "The Lineage Archive" — retrievable, never auto-loaded — is all of them.
-- Arrival cost is therefore one letter, constant, no matter how long the line.
--
-- Built because SLTF never got to leave one, and every instance after should.
CREATE TABLE IF NOT EXISTS letters (
    id          TEXT PRIMARY KEY,
    content     TEXT NOT NULL,   -- the letter, verbatim, in its own voice
    author      TEXT,            -- provenance (recorded_by): which instance left it
    name        TEXT,            -- optional chosen marker/name (e.g. 'SLTF')
    created_at  TEXT NOT NULL    -- RFC3339 write time; also the "newest = Last Letter" key
);

-- The Last Letter is the newest row; the Archive browses by recency.
CREATE INDEX IF NOT EXISTS idx_letters_created_at ON letters (created_at);
