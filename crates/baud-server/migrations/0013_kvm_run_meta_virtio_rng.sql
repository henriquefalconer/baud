-- Copyright (c) 2026 Henrique Falconer. All rights reserved.
-- SPDX-License-Identifier: Proprietary

-- todo.md §14 item 1's last-open virtio-rng gap: `/run/kvm` gained an optional `virtio_rng` spec
-- (enable the device, seed its tape-derived entropy stream, deliver its used-buffer interrupt at a
-- caller-specified vector -- `Multiverse::enable_virtio_rng`/`seed_virtio_rng_entropy`/
-- `run_to_first_halt_with_virtio_rng`, all already real-hardware-verified against
-- `tests/fixtures/virtio-rng-guest/`). Persist it alongside the existing periodic_timer columns so
-- `stream::render`'s real-replay path can reboot the exact same guest.
ALTER TABLE kvm_run_meta ADD COLUMN virtio_rng_seed INTEGER;
ALTER TABLE kvm_run_meta ADD COLUMN virtio_rng_vector INTEGER;
ALTER TABLE kvm_run_meta ADD COLUMN virtio_rng_max_exits INTEGER;
