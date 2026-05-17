CREATE TABLE IF NOT EXISTS runs (
    id          TEXT PRIMARY KEY,
    repo        TEXT NOT NULL,
    branch      TEXT NOT NULL,
    "commit"    TEXT NOT NULL,
    clone_url   TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'queued',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS steps (
    run_id      TEXT NOT NULL REFERENCES runs(id),
    name        TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'pending',
    started_at  TEXT,
    finished_at TEXT,
    PRIMARY KEY (run_id, name)
);

CREATE TABLE IF NOT EXISTS log_lines (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id      TEXT NOT NULL REFERENCES runs(id),
    step        TEXT NOT NULL,
    line        TEXT NOT NULL,
    created_at  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_log_lines_run_step ON log_lines(run_id, step);
CREATE INDEX IF NOT EXISTS idx_runs_status ON runs(status);
