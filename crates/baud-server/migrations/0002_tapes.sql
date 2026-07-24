-- baud-server M1: tape lifecycle tracking

CREATE TABLE IF NOT EXISTS tapes (
    id              TEXT PRIMARY KEY,
    backend         TEXT NOT NULL DEFAULT 'local',   -- 'local' | 'daytona'
    state           TEXT NOT NULL DEFAULT 'running', -- 'creating'|'running'|'stopped'|'archived'|'deleted'
    vcpus           INTEGER NOT NULL DEFAULT 1,
    memory_mib      INTEGER NOT NULL DEFAULT 1024,
    disk_mib        INTEGER NOT NULL DEFAULT 1024,
    auto_stop_secs  INTEGER NOT NULL DEFAULT 60,
    auto_archive_secs INTEGER NOT NULL DEFAULT 300,
    image           TEXT,
    preview_url     TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_tapes_state ON tapes(state);
CREATE INDEX IF NOT EXISTS idx_tapes_backend ON tapes(backend);
