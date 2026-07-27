-- Copyright (c) 2026 Henrique Falconer. All rights reserved.
-- SPDX-License-Identifier: Proprietary

-- todo.md §14 item 5's remaining "boot/cmdline/CLI wiring" gap for virtio-blk: `/run/kvm` gained an
-- optional `virtio_blk` spec (attach a read-only content-addressed base image + in-memory
-- copy-on-write overlay, deliver its used-buffer interrupt at a caller-specified vector --
-- `Multiverse::enable_virtio_pci_blk`/`run_to_first_halt_with_virtio_pci_blk`, already
-- real-hardware-verified against `tests/fixtures/linux-guest/virtio_blk_init.c`). Persist it
-- alongside the existing virtio_rng columns so `stream::render`'s real-replay path can reboot the
-- exact same guest. `virtio_blk_image_path` (not the image bytes) is stored, mirroring
-- `initramfs_path`'s own path-not-content convention -- a real disk image can be far larger than an
-- initramfs.
ALTER TABLE kvm_run_meta ADD COLUMN virtio_blk_image_path TEXT;
ALTER TABLE kvm_run_meta ADD COLUMN virtio_blk_vector INTEGER;
ALTER TABLE kvm_run_meta ADD COLUMN virtio_blk_max_exits INTEGER;
