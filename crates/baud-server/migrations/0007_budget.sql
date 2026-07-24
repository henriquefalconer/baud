-- M9: per-run budget accounting and shrink results
CREATE TABLE IF NOT EXISTS run_budget (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id      TEXT NOT NULL,
    sandbox_minutes REAL NOT NULL DEFAULT 0.0,
    recorded_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE IF NOT EXISTS shrink_results (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id          TEXT NOT NULL UNIQUE,
    original_steps  INTEGER NOT NULL,
    shrunk_steps    INTEGER NOT NULL,
    passes_applied  TEXT NOT NULL,
    fault_schedule  TEXT,               -- JSON blob describing the minimal fault sequence
    created_at      INTEGER NOT NULL DEFAULT (unixepoch())
);
