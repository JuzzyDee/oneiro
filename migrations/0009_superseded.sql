-- migrations/0009_superseded.sql — CLA-117 supersession.
--
-- Knowledge gets overturned, not just extended. When a new episodic provides
-- a reliable position update that invalidates an existing semantic (a clean
-- supersession — distinct from a genuine still-open conflict, which is held
-- honestly *inside* the semantic), the old view is flagged `superseded`: kept
-- in the store for completeness, but excluded from tool surfacing — the same
-- end state as forgotten. `superseded_by` points at the semantic that
-- replaced it (a cheap forensic trace of why the position changed).
--
-- Apply with: wrangler d1 migrations apply oneiro-db

ALTER TABLE memories ADD COLUMN superseded    INTEGER NOT NULL DEFAULT 0;  -- 1 = no longer believed; not surfaced
ALTER TABLE memories ADD COLUMN superseded_by TEXT;                         -- id of the semantic that replaced it
