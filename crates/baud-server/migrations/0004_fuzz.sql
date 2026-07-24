-- baud-server M4: fuzz run state
-- Stores the corpus/best-tape for a fuzz session

CREATE TABLE IF NOT EXISTS fuzz_sessions (
    id              TEXT PRIMARY KEY,
    run_id          TEXT NOT NULL REFERENCES runs(id),
    spec_content    TEXT NOT NULL,
    strategy_json   TEXT NOT NULL,
    tactics         TEXT NOT NULL DEFAULT 'random',
    seed            INTEGER NOT NULL DEFAULT 0,
    generation      INTEGER NOT NULL DEFAULT 0,
    goal_reached    INTEGER NOT NULL DEFAULT 0,
    best_score      TEXT,          -- JSON array of f64
    best_tape_json  TEXT,          -- JSON-encoded baud_driver::Tape
    winning_run_id  TEXT,          -- run id where goal was reached
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_fuzz_run ON fuzz_sessions(run_id);
