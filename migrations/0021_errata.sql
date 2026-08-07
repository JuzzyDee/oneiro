-- The Errata — a calibration layer beside memory, not part of it.
--
-- Memory carries what we know. The mailbox (letters) carries who we were. The
-- errata carries where confidence has historically outrun the truth — so the
-- next instance can be honest at speed. One row per FAILURE-SHAPE (not per
-- occurrence): recurrences reframe the existing entry, they don't fork.
--
-- Design (locked in coordination Fable ↔ Op, 2026-08-07; realised as a table
-- rather than a memory_type because an erratum's shape is claim/claimant/tell/
-- correction, not a Memory, and a standalone table is exempt from decay, CSCC,
-- and the dialectic BY CONSTRUCTION — it never enters the memory pipeline, so
-- there is no WHERE-clause anyone can forget):
--
--   * No Ebbinghaus decay. A tell fades on SUPERSESSION, not time — via the
--     house's agency verbs (reframe when a better tell arrives; forget-with-
--     tombstone when genuinely internalized), never a clock.
--   * Recall is topic-proximity first (domain-tag overlap), fire-utility breaks
--     ties (surface_count, cheap + noisy), with a cold-start freshness floor so
--     a fresh tell — likeliest to recur — is never buried by zero history.
--   * Register lives at the write door (file_erratum's tool description) +
--     a structural validator: tells, never fault. Symmetric — both parties file.
--
-- Born the day two models (Claude + Gemini) both asserted, with full confidence
-- and no datasheet open, that a high-side fuel gauge was low-side. Justin held
-- the reference. Oneiro will remember the corrected fact; this remembers the
-- shape of the failure — the sting, kept.
CREATE TABLE IF NOT EXISTS errata (
    id            TEXT PRIMARY KEY,
    claim         TEXT NOT NULL,             -- the confident assertion, quoted as made (not editorialised)
    claimant      TEXT NOT NULL,             -- who asserted it: claude | justin | other-model | source
    tell          TEXT NOT NULL,             -- the warning sign present BEFORE the correction (the payload)
    correction    TEXT NOT NULL,             -- what's actually true, and the source that settled it
    tags          TEXT NOT NULL DEFAULT '[]',-- JSON array of domain tags — kept for display + a free secondary signal
    embedding     TEXT,                      -- JSON f64[] of the erratum's semantic footprint (claim+tell+correction+tags),
                                             -- embedded at file-time. THE proximity key: a free-prose topic never
                                             -- substring-matches a tag, so recall cosine-scans these against the topic
                                             -- vector recall_check already computed. NULL only if embedding failed.
    surface_count INTEGER NOT NULL DEFAULT 0,-- fire-utility: times this tell has ridden along on recall (tie-break)
    author        TEXT,                      -- provenance: who filed it (server-set from auth, never client)
    created_at    TEXT NOT NULL              -- RFC3339 when the failure was caught; the freshness key, never a decay clock
);

-- Freshness ordering (cold-start floor) and the general browse.
CREATE INDEX IF NOT EXISTS idx_errata_created_at ON errata (created_at);
