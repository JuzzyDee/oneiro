-- Beacon image index (the "shelf manifest" / oven state). One row per image:
-- which memory, where the PNG lives, when baking started, lifecycle status,
-- and when delivered.
--   status: 'baking'  — generation in flight (row written BEFORE the bake, so
--                        an in-progress bake is visible and never piles up)
--           'ready'   — PNG stored, waiting on the shelf
--           'served'  — delivered to the device
--           'failed'  — generation errored (caught)
-- A 'baking' row older than the staleness window is presumed dead (the job
-- crashed) and re-triggered. The PNG bytes live in R2 under beacon/; this table
-- is the queryable state + provenance over them.
CREATE TABLE IF NOT EXISTS beacon_images (
    id           TEXT PRIMARY KEY,
    memory_id    TEXT NOT NULL,
    r2_key       TEXT,                       -- NULL until the bake completes
    generated_at TEXT NOT NULL,              -- baking-start time; also staleness clock
    status       TEXT NOT NULL DEFAULT 'baking',
    served_at    TEXT
);

-- Delivery grabs the latest 'ready' row; the baker scans 'baking' freshness.
CREATE INDEX IF NOT EXISTS idx_beacon_images_status
    ON beacon_images (status, generated_at);
