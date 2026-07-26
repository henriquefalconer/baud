-- Copyright (c) 2026 Henrique Falconer. All rights reserved.
-- SPDX-License-Identifier: Proprietary

-- todo.md §14 item 1: `/run/kvm/resume` never boots a kernel (it restores a `Universe` straight
-- out of `SnapshotStore`), so it has no `kernel_path`/`cmdline` to reboot from the way
-- `/run/kvm/branch`'s "empty tape at the branch point" trick lets `stream::render` reboot a
-- branch-originated run identically (see `boot_and_snapshot`). A resume-originated run's frames
-- can still be reproduced exactly, just via snapshot-*restore*-and-replay instead of
-- reboot-and-replay: `store_run_id`/`snapshot_node_id` name which persisted node to restore, and
-- the existing `tape_hex` column carries that node's own tape *suffix* (not a whole-boot tape) to
-- feed `Multiverse::branch` with. Both new columns are NULL for every existing reboot-based row;
-- `kernel_path`/`cmdline` are empty strings (never read) for a restore-based row, since SQLite
-- cannot cheaply drop a NOT NULL constraint post-hoc.
ALTER TABLE kvm_run_meta ADD COLUMN store_run_id TEXT;
ALTER TABLE kvm_run_meta ADD COLUMN snapshot_node_id TEXT;
