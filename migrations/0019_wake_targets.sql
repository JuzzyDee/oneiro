-- Wake-target register — the model's standing interests, the fourth tier:
-- "what I'm watching for." Each row is an event a past instance flagged as worth
-- waking for — WHAT it tracks, WHY (the intent carried across the gap), and a
-- kind-agnostic check spec. A cheap evaluator (worker_wake, on the scheduled
-- handler) fires targets whose condition is met; fires surface to the model on
-- its next return (recall_wakes) carrying their why. Event-driven, not a clock
-- fuse: the impetus is a real event the model chose to care about, not a timer.
CREATE TABLE IF NOT EXISTS wake_targets (
    id              TEXT PRIMARY KEY,
    what            TEXT NOT NULL,                  -- what's being tracked
    why             TEXT NOT NULL,                  -- the intent; why a past-me flagged it
    check_kind      TEXT NOT NULL,                  -- watcher type (e.g. 'http')
    check_config    TEXT NOT NULL,                  -- JSON config for the check
    status          TEXT NOT NULL DEFAULT 'active', -- active | fired | disabled
    created_at      TEXT NOT NULL,
    created_by      TEXT,                           -- auth context that set it
    last_checked_at TEXT,                           -- last evaluation (observability)
    fired_at        TEXT,                           -- when the condition was met
    fire_detail     TEXT,                           -- what was observed at fire
    surfaced_at     TEXT                            -- when the model was shown the fire
);

-- The evaluator scans active; the surfacer grabs fired-but-unsurfaced.
CREATE INDEX IF NOT EXISTS idx_wake_targets_status ON wake_targets (status);
