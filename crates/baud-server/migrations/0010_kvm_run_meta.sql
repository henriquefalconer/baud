-- baud-server M11: persist which kernel/cmdline/tape produced a real /run/kvm boot's frames,
-- so POST /runs/:id/stream/render can replay a real KVM run instead of only ever fabricating
-- synthetic pixels from a stored hash (todo.md §14, "eighteenth brick"'s "Not yet done" (1)/(2)).

CREATE TABLE IF NOT EXISTS kvm_run_meta (
    run_id      TEXT PRIMARY KEY REFERENCES runs(id),
    kernel_path TEXT NOT NULL,
    cmdline     TEXT NOT NULL,
    tape_hex    TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);
