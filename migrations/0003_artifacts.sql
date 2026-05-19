CREATE TABLE IF NOT EXISTS artifacts (
    id         TEXT PRIMARY KEY,
    run_id     TEXT NOT NULL REFERENCES runs(id),
    step       TEXT NOT NULL,
    path       TEXT NOT NULL,
    size       INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE (run_id, step, path)
);

CREATE INDEX IF NOT EXISTS idx_artifacts_run ON artifacts(run_id);
