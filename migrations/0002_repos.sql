CREATE TABLE IF NOT EXISTS repos (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    clone_url  TEXT NOT NULL UNIQUE,
    branch     TEXT NOT NULL DEFAULT 'main'
);
