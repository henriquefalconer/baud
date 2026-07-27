-- Copyright (c) 2026 Henrique Falconer. All rights reserved.
-- SPDX-License-Identifier: Proprietary

-- todo.md §14 item 5(c)'s last-open gap: `/run/kvm` gained an optional `acpi` flag
-- (`Multiverse::write_acpi_tables` -- RSDP -> XSDT -> FADT + DSDT + MADT-with-one-LAPIC). Persist
-- it alongside the existing periodic_timer/virtio_rng columns so `stream::render`'s real-replay
-- path can reboot the exact same guest.
ALTER TABLE kvm_run_meta ADD COLUMN acpi INTEGER NOT NULL DEFAULT 0;
