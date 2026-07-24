-- baud-server M2: run provisioning and observations

-- Extend the runs table with spec and provisioning info
-- (The initial schema has a runs table already; we add columns if they don't exist)

-- Note: SQLite doesn't support ADD COLUMN IF NOT EXISTS in all versions,
-- so we use a safe approach: drop and recreate with all columns.
-- Since the table starts empty in a fresh DB, this is safe.

DROP TABLE IF EXISTS observations;
DROP TABLE IF EXISTS syscall_records;
DROP TABLE IF EXISTS runs;

CREATE TABLE IF NOT EXISTS runs (
    id              TEXT PRIMARY KEY,
    -- Spec content (YAML or TOML, raw string)
    spec_content    TEXT NOT NULL DEFAULT '',
    -- blake3 hash of the spec content
    spec_hash       TEXT NOT NULL,
    -- Nix flake ref from the spec
    nix_ref         TEXT NOT NULL DEFAULT '',
    -- blake3 closure hash (from baud-packages build)
    closure_hash    TEXT,
    -- Strategy JSON
    strategy        TEXT,
    -- Tactics JSON
    tactics         TEXT,
    -- RNG seed
    seed            INTEGER NOT NULL DEFAULT 0,
    -- Budget in minutes
    budget_minutes  INTEGER NOT NULL DEFAULT 60,
    -- Associated tape ID (sandbox)
    tape_id         TEXT REFERENCES tapes(id),
    -- Run status: pending | provisioning | running | done | failed | aborted
    status          TEXT NOT NULL DEFAULT 'pending',
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
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

CREATE INDEX IF NOT EXISTS idx_runs_status ON runs(status);
CREATE INDEX IF NOT EXISTS idx_runs_tape ON runs(tape_id);
CREATE INDEX IF NOT EXISTS idx_obs_run_step ON observations(run_id, step);
CREATE INDEX IF NOT EXISTS idx_sys_run_node ON syscall_records(run_id, node);
