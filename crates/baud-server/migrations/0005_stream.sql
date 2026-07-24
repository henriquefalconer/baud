-- baud-server M5: frame records and net weather events

CREATE TABLE IF NOT EXISTS frame_records (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id      TEXT NOT NULL REFERENCES runs(id),
    node        INTEGER NOT NULL,
    step        INTEGER NOT NULL,
    width       INTEGER NOT NULL,
    height      INTEGER NOT NULL,
    format      TEXT NOT NULL,    -- "rgba8888" | "rgb565" | "indexed8"
    hash        BLOB NOT NULL,    -- 32-byte blake3 hash (plaintext)
    -- bytes are NOT stored here (regenerated from tape replay)
    recorded_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS net_events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id      TEXT NOT NULL REFERENCES runs(id),
    step        INTEGER NOT NULL,
    kind        TEXT NOT NULL,    -- "partition_on" | "partition_off" | "delay" | "drop"
    -- Optional fields for delay/drop events
    from_node   INTEGER,
    to_node     INTEGER,
    delay_ticks INTEGER,
    drop_prob   REAL,
    recorded_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_frames_run_node ON frame_records(run_id, node, step);
CREATE INDEX IF NOT EXISTS idx_net_run_step ON net_events(run_id, step);
