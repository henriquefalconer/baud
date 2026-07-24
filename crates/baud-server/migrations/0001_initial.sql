-- baud-server initial schema (M0 skeleton)

CREATE TABLE IF NOT EXISTS runs (
    id          TEXT PRIMARY KEY,
    spec_hash   TEXT NOT NULL,
    closure_hash TEXT,
    seed        INTEGER NOT NULL,
    status      TEXT NOT NULL DEFAULT 'pending',
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS observations (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id      TEXT NOT NULL REFERENCES runs(id),
    step        INTEGER NOT NULL,
    node        INTEGER NOT NULL,
    probe       TEXT NOT NULL,
    value       BLOB NOT NULL,
    recorded_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS syscall_records (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id      TEXT NOT NULL REFERENCES runs(id),
    node        INTEGER NOT NULL,
    sysno       INTEGER NOT NULL,
    args_digest BLOB NOT NULL,
    ret         INTEGER NOT NULL,
    vtime       INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_obs_run_step ON observations(run_id, step);
CREATE INDEX IF NOT EXISTS idx_sys_run_node ON syscall_records(run_id, node);
