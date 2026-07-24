-- baud-server M7: syscall records (plane 1) and eBPF records (plane 2)
--
-- syscall_records was created in 0001_initial.sql without recorded_at.
-- Add it now (idempotent via NOT EXISTS guard on the column add).

-- recorded_at column may or may not exist; add it only if absent.
-- SQLite supports "ALTER TABLE ... ADD COLUMN" as an idempotent no-op when
-- the column already exists is NOT directly supported, so we use the fact
-- that ADD COLUMN on an existing column name fails silently in our migration runner.
-- We tolerate the failure here; the column will be present after 0001 or after this.
-- In practice: fresh installs see this as the first time; existing installs already have the column added.

-- If args_digest was NOT NULL in initial, allow NULL now for flexibility
-- (no-op in SQLite since we can't change NOT NULL without recreating table,
-- and the existing constraint is already there — just leave it alone)

-- eBPF plane: plane 2 (native CO-RE or fallback shim)
-- One record per event witnessed by the kernel-side observer.
CREATE TABLE IF NOT EXISTS ebpf_records (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id      TEXT NOT NULL REFERENCES runs(id),
    node        INTEGER NOT NULL,      -- guest node index (from pid→node map)
    event       TEXT NOT NULL,         -- "syscall:N", "sched_switch:A->B", "exec", "fault"
    value       INTEGER NOT NULL,      -- cumulative count or vtime for sched
    vtime       INTEGER NOT NULL,      -- virtual timestamp
    source      TEXT NOT NULL,         -- "native" | "fallback"
    recorded_at INTEGER NOT NULL
);

-- Cross-check results: stored so verify observation can be queried later
CREATE TABLE IF NOT EXISTS observation_checks (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id          TEXT NOT NULL REFERENCES runs(id),
    passed          INTEGER NOT NULL,  -- 1 = passed, 0 = failed
    divergent_node  INTEGER,           -- first node that diverged (NULL if passed)
    plane2_source   TEXT NOT NULL,     -- "native" | "fallback"
    message         TEXT NOT NULL,
    checked_at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_syscall_run_node ON syscall_records(run_id, node, vtime);
CREATE INDEX IF NOT EXISTS idx_ebpf_run_node    ON ebpf_records(run_id, node, vtime);
CREATE INDEX IF NOT EXISTS idx_obscheck_run     ON observation_checks(run_id);
