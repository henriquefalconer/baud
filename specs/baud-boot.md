<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Boot Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-25

---

## 1. Overview

### Purpose

`baud-boot` is the deterministic guest-boot subsystem: it turns a real Linux `bzImage` + initramfs into a
running userspace that is a pure function of the tape, on baud's existing single-vCPU deterministic machine.
It owns the Linux/x86 `struct boot_params` "zero page", the E820 memory map, the deterministic kernel command
line, the `SETUP_RNG_SEED` entropy seed, and the guest's single, trappable shutdown event. It is the bridge
between the machine (`baud-multiverse`) and a real image (`baud-packages`). Implemented in
`crates/baud-multiverse/src/linux/bootparams.rs`.

### Goals

- **Direct boot** a real bzImage + initramfs — no BIOS, UEFI, or bootloader — via `linux-loader`.
- **Pin every boot-time nondeterminism source**: address layout (`nokaslr`), the CRNG seed, the memory map,
  the clocksource, and the exit path.
- **Reach `/init`** in userspace, then produce exactly one VMM-detectable shutdown.

### Non-Goals

- Building the image (that is `baud-packages`, `specs/baud-packages.md`).
- Firmware / ACPI emulation beyond an optional S5 shutdown port.
- SMP / multi-vCPU bring-up (the machine is single-vCPU by construction).

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────┐
│        baud-multiverse :: linux::bootparams    │
│  bzImage load ─ zero-page ─ E820 ─ cmdline     │
│  SETUP_RNG_SEED ─ 64-bit entry ─ shutdown trap │
└───────────────────┬──────────────────────────┘
      ▲ image (bzImage + initramfs) from baud-packages
      │ deterministic seed byte drawn off the tape
      ▼ runs on the single-vCPU machine (baud-vcpu)
```

### Rationale

- A module of `baud-multiverse`, not a standalone crate: it manipulates the same `GuestMemory` and vCPU the
  run loop owns. Deps = `{linux-loader 0.14, vm-memory 0.18, baud-proto}`. Soft budget ≤ 700 LOC.
- Everything here is pure "prepare the initial state" logic — no run loop, no devices — so it is
  unit-testable against an in-memory `GuestMemory` without KVM.

### Types & API

```rust
pub struct BootConfig<'a> {
    pub kernel: &'a Path,        // a real bzImage
    pub initramfs: &'a Path,     // gzip'd newc cpio
    pub cmdline: &'a str,        // §5; NUL-terminated when written
    pub rng_seed: [u8; 32],      // §6; a deterministic draw off the tape
    pub mem_bytes: u64,          // guest RAM size → E820 (§4)
}

pub struct LoadedBoot {
    pub entry_rip: GuestAddress, // 64-bit entry = protected-mode base + 0x200
    pub boot_params: GuestAddress,
}

impl<'a> BootConfig<'a> {
    /// Load the kernel + initramfs, write the zero page, E820, cmdline, and RNG
    /// seed into `mem`, and return the entry point + boot_params pointer.
    pub fn load<M: GuestMemory>(&self, mem: &M) -> Result<LoadedBoot, BootError>;
}
```

---

## 3. The zero page (`struct boot_params`)

`linux-loader`'s `BzImage::load` places the protected-mode kernel high (at `0x100000`) and parses the image
`setup_header`; `baud-boot` writes the remaining fields the 64-bit boot protocol requires
(`Documentation/x86/boot.rst`). All offsets are within the zero page:

| Field | Offset | Value |
|-------|--------|-------|
| `setup_header` (copied from image) | `0x01F1` | the image's own header, verbatim (`boot_flag=0xAA55`, `"HdrS"`) |
| `type_of_loader` | `0x210` | `0xFF` (undefined loader) |
| `loadflags` | `0x211` | bit0 `LOADED_HIGH` + bit7 `CAN_USE_HEAP`; clear `QUIET_FLAG` |
| `heap_end_ptr` | `0x224` | `0xFE00` (with `CAN_USE_HEAP`) |
| `cmd_line_ptr` | `0x228` | phys addr of the cmdline string (§5) |
| `ramdisk_image` / `ramdisk_size` | `0x218` / `0x21C` | initramfs load addr / byte length |
| `setup_data` | `0x250` | phys addr of the RNG-seed node (§6) |
| `e820_entries` / `e820_table` | `0x1E8` / `0x2D0` | count / entries (§4) |

```rust
const HDR_OFF: u64 = 0x1F1;
hdr.type_of_loader = 0xFF;
hdr.loadflags = LOADED_HIGH | CAN_USE_HEAP;      // 0x01 | 0x80
hdr.heap_end_ptr = 0xFE00;
hdr.cmd_line_ptr = CMDLINE_ADDR as u32;
hdr.ramdisk_image = INITRD_ADDR as u32;
hdr.ramdisk_size  = initramfs_len as u32;
hdr.setup_data    = RNG_SEED_ADDR;               // 64-bit, offset 0x250
mem.write_obj(hdr, GuestAddress(ZERO_PAGE + HDR_OFF))?;
```

The vCPU enters in 64-bit long mode at `protected-mode base + 0x200` (`0x100200`), with identity-mapped low
memory, a flat GDT, interrupts disabled, and `%rsi = boot_params` — all set by the machine's vCPU init
(`baud-vcpu`); `baud-boot` only supplies `entry_rip` and the `boot_params` pointer.

---

## 4. The E820 memory map

Omitting E820 is the classic from-scratch-VMM silent hang — the kernel finds no usable RAM and dies before
the console inits. `baud-boot` publishes one usable low region and one high region, leaving the MMIO / tape
window out of the map:

```rust
const E820_RAM: u32 = 1;
let mut e820 = [boot_e820_entry::default(); 128];
e820[0] = entry(0x0000_0000, 0x0009_FC00, E820_RAM);                    // low RAM (< 640K)
e820[1] = entry(0x0010_0000, self.mem_bytes - 0x0010_0000, E820_RAM);   // main RAM
// (the tape MMIO window, if any, is deliberately not published as RAM)
write_e820(mem, &e820[..2])?;   // → boot_params.e820_table@0x2D0, count@0x1E8
```

Entries are `{ addr: u64, size: u64, kind: u32 }`; usable RAM is `kind = 1`. The map is a fixed function of
`mem_bytes`, so it never varies run to run.

---

## 5. Deterministic command line

One fixed string; `baud image lint` (`baud-packages`) rejects any image whose spec would change it:

```
console=ttyS0 nokaslr nosmp maxcpus=1 clocksource=tsc tsc=reliable no-kvmclock
no_timer_check pci=off acpi=off reboot=t panic=-1 quiet loglevel=1 printk.time=0
random.trust_cpu=off random.trust_bootloader=on i8042.noaux i8042.nomux
i8042.nopnp 8250.nr_uarts=1 nomodule rdinit=/init
```

| Token | Effect on determinism |
|-------|-----------------------|
| `nokaslr` | fixed kernel / module / stack addresses (paired with `# CONFIG_RANDOMIZE_BASE`) |
| `nosmp maxcpus=1` | one logical CPU — no AP bring-up, no cross-CPU scheduling variance |
| `clocksource=tsc tsc=reliable no-kvmclock` | time is the machine's deterministic TSC only |
| `no_timer_check` | skip the IRQ0 routing probe (pokes hardware baud does not model) |
| `pci=off acpi=off` | no bus / table probing |
| `reboot=t panic=-1` | immediate triple-fault exit the VMM traps (no timed wait) |
| `random.trust_bootloader=on` | credit the `SETUP_RNG_SEED` so the CRNG seeds synchronously (§6) |
| `random.trust_cpu=off` | do not credit RDRAND / RDSEED into the pool |
| `printk.time=0 quiet loglevel=1` | minimize console-ordering variance |
| `rdinit=/init` | run `/init` from the initramfs directly, no root-device search |

---

## 6. Boot entropy seed (`SETUP_RNG_SEED`)

The one boot-seed transport for x86 direct boot. `baud-boot` writes a `setup_data` node whose 32-byte payload
is a deterministic draw off the tape's entropy sub-stream (the same stream that serves `rdrand`,
`specs/baud-multiverse.md` §7), and points `boot_params.setup_data@0x250` at it:

```rust
#[repr(C)]
struct SetupData { next: u64, kind: u32, len: u32, data: [u8; 32] }
const SETUP_RNG_SEED: u32 = 9;

let node = SetupData { next: 0, kind: SETUP_RNG_SEED, len: 32, data: self.rng_seed };
mem.write_obj(node, GuestAddress(RNG_SEED_ADDR))?;   // referenced by hdr.setup_data
```

The kernel mixes it via `add_bootloader_randomness()` and **zeroes it in place** (forward secrecy), so it
must be re-written on every boot and every snapshot-restore. With `random.trust_bootloader=on` the seed is
credited, the CRNG initializes synchronously and identically, and the kernel's `getrandom()` wait path never
runs — so an unmodified guest's `getrandom` / `/dev/urandom` are a pure function of the tape (validated at
`todo.md` H7).

**Kernel-version caveat.** `SETUP_RNG_SEED` (v6.0), `random.trust_bootloader` (v5.4), and `random.trust_cpu`
(v4.19) are all *modern-kernel* mechanisms. On an **older distro kernel** (e.g. Ubuntu 18.04's 4.15, `todo.md`
§4.7) none of them exist, so this seed node is a no-op and `baud-boot` may omit it. Determinism there does not
depend on a credited seed — it comes entirely from the machine pinning the CRNG's *inputs* (trapped
RDTSC/RDRAND, rewritten RDSEED, exact-boundary interrupt injection, zeroed RAM, pinned MAC), which the
pre-5.17 CRNG folds into its key and credits via `add_interrupt_randomness`. baud writes the seed node only
when the target kernel honors it; either way `getrandom` stays a pure function of the tape.

---

## 7. Shutdown detection

`/init` (`specs/baud-packages.md`) ends with `reboot(RB_POWER_OFF)`; `baud-boot` recognizes the guest's exit
as a single event and stops the vCPU:

- **Primary**: `reboot=t` makes the power-off path triple-fault → `KVM_EXIT_SHUTDOWN` (unambiguous with a
  single vCPU and no ACPI).
- **Optional (clean vs. crash)**: advertise an ACPI FADT with a PM1a control port; the kernel's S5 write
  (`SLP_EN`) becomes a `KVM_EXIT_IO` the VMM reads as a clean poweroff, distinct from a fault.

```rust
match vcpu.run()? {
    VcpuExit::Shutdown                                   => Outcome::GuestExited,   // reboot=t path
    VcpuExit::IoOut(port, d) if port == PM1A_CNT && is_s5(d) => Outcome::CleanPoweroff,
    other                                                => dispatch(other),        // specs/baud-vcpu.md
}
```

---

## 8. Determinism Properties

- The zero page, E820, cmdline, and seed node are fixed functions of `(image, mem_bytes, tape)` — no host
  value enters the guest's initial state.
- The RNG-seed node is re-materialized on every boot and every snapshot-restore (the kernel zeroes it).
- No wall clock, RTC, HPET, PCI, or ACPI probe touches host state; time is the machine's TSC alone.
- The exit is a single, position-stable event, never a timed wait.

---

## 9. Testing

```rust
#[test] fn guest_kernel_boots_to_userspace() {
    let out = boot(real_image(), tape());
    assert!(out.console.contains("MARKER_BOOT_OK") && out.exit == Outcome::GuestExited);
}

#[test] fn boot_params_seed_is_pinned() {
    let (a, b) = (boot(real_image(), tape.clone()), boot(real_image(), tape.clone()));
    assert_eq!(a.seed_node_bytes, b.seed_node_bytes);          // identical SETUP_RNG_SEED
    assert_eq!(a.first_getrandom, b.first_getrandom);          // ⇒ reproducible CRNG init
}

#[test] fn e820_absence_is_a_hard_error() {
    // a build that forgets the map must fail loud, not hang
    assert!(matches!(BootConfig { mem_bytes: 0, ..cfg() }.load(&mem), Err(BootError::NoUsableRam)));
}

#[test] fn init_powers_off_deterministically() {
    let (a, b) = (boot(real_image(), tape.clone()), boot(real_image(), tape.clone()));
    assert_eq!(a.exit_pc, b.exit_pc);                          // identical shutdown point
}
```

The `drive/h7.sh` script wraps these: build a real image → `guest_kernel_boots_to_userspace` →
`boot_params_seed_is_pinned` → `double_boot_ram_hash_identical` (`baud-multiverse`) → `os_entropy_is_deterministic`.

---

## 10. Security Considerations

| Concern | Handling |
|---------|----------|
| Host entropy leaking via the boot seed | The seed is a tape draw; never a host / firmware value |
| Guest reads host memory via a bad E820 | Only baud-owned RAM regions are published; MMIO carved out |
| Malformed image hangs the VMM | Missing header / E820 / entry is `Err(BootError)`, never a silent hang |

---

## 11. Future Considerations

| Feature | Description |
|---------|-------------|
| PVH / `SETUP_PVH` boot | An alternate, lighter entry protocol for PVH-capable kernels |
| Multiple `setup_data` nodes | Chain `SETUP_DTB` / `SETUP_EFI` alongside the RNG seed if ever needed |
| Snapshot fast-path | Skip re-loading the bzImage on restore; only re-materialize the seed node |
