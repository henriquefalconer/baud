-- Copyright (c) 2026 Henrique Falconer. All rights reserved.
-- SPDX-License-Identifier: Proprietary

-- todo.md §14 item 1: `/run/kvm` gained an optional `initramfs_path` (a real Linux guest image
-- ships kernel + initramfs separately, spec §4.2/§4.3) and an optional `periodic_timer` spec (a
-- real Linux kernel's own scheduler calibration hangs forever under a plain boot-to-halt with no
-- injected interrupts, unlike every hand-assembled fixture in this workspace before it) — persist
-- both alongside the existing kernel_path/cmdline/tape_hex so `stream::render`'s real-replay path
-- can reboot the exact same guest, not just the exact same kernel+tape.
ALTER TABLE kvm_run_meta ADD COLUMN initramfs_path TEXT;
ALTER TABLE kvm_run_meta ADD COLUMN periodic_timer_period_rcb INTEGER;
ALTER TABLE kvm_run_meta ADD COLUMN periodic_timer_vector INTEGER;
ALTER TABLE kvm_run_meta ADD COLUMN periodic_timer_max_ticks INTEGER;
