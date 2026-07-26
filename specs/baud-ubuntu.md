<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Ubuntu Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-25

---

## 1. Overview

### Purpose

`examples/ubuntu` is a validation target, not infrastructure: it boots a **full, unmodified Ubuntu 18.04.1
LTS** distro (stock 4.15 kernel + stock cloud rootfs) under baud to the serial login prompt, then proves the
whole-machine execution is deterministic with a **cross-VM fingerprint** — two independent VMs on the same
`(image, tape)` produce byte-identical `deterministic events`, `guest RIP`, `guest physical`, and `guest
memory hash`. It demonstrates that baud's determinism holds for real, unmodified system software (systemd,
udev, glibc, a distro kernel), not just purpose-built guests, and that it holds even **nested inside WSL2**.
It exercises the same machine, boot pipeline, and CLI as any workload — nothing here is Ubuntu-specific in a
crate.

### Goals

- **A real distro, unmodified**: stock Ubuntu kernel + rootfs, booted to `ubuntu login:` on the serial console.
- **Provable determinism**: a timed-exit fingerprint that is byte-identical across two independent VMs.
- **Runs nested under WSL2**: baud's L2 guest on the L1 WSL2 dev host (`todo.md` §4.7, §9), no bare-metal
  requirement for the boot itself.
- **Zero special treatment**: deployed via `examples/ubuntu/spec.toml`; no distro knowledge in any crate.

### Non-Goals

- Logging in / running workloads inside Ubuntu (the goal is a deterministic boot-to-login fingerprint; a
  workload harness is `baud-guest-harness` + a separate example).
- Customizing the rootfs (only cmdline + a mask list + hypervisor-side pins; no source edits).
- Guaranteeing the **PMU branch counter** is deterministic under nested WSL2 — that is validated at H0
  (`rcb_is_deterministic_on_this_cpu`); if the L1 PMU is nondeterministic, the fingerprint run falls back to
  bare metal (§3, §10).

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────────────────┐
│   examples/ubuntu/  (a TARGET, not a crate)                │
│  spec.toml → stock 18.04.1 bzImage + initrd + raw rootfs   │
│  cmdline   → deterministic tokens + systemd.mask=…         │
│  fingerprint → timed-exit dump (events/RIP/GPA/RAM hash)   │
└──────────────────────────────────────────────────────────┘
   ▲ boots on the machine via baud-boot; disk = deterministic virtio-blk
   ▲ additions vs the minimal guest: minimal ACPI + PCI + virtio-blk
```

### Rationale

- No new crate and no vendored distro — the guest is the upstream Ubuntu image, unmodified. The only shipped
  artifacts are `spec.toml`, the cmdline/mask list, and the drive script. Booting is `baud-boot`; the disk is
  the machine's deterministic virtio-blk; the fingerprint is a `baud-multiverse` capability reused by the CLI.

---

## 3. The nesting model (L0 / L1 / L2)

Booting Ubuntu under baud on the Windows dev machine is a **three-level** nesting; "L2" names the Ubuntu
guest:

```
L0  Hyper-V              — Windows root hypervisor (VBS); exposes nested VMX to L1
 └ L1  WSL2 Ubuntu       — where baud runs; /dev/kvm present (nestedVirtualization=true)
     └ L2  Ubuntu 18.04.1 under baud/KVM   ← this spec
         vm0, vm1 = two L2 VMs, one (image, tape), one fingerprint
```

- **Feasibility**: WSL2 (L1) exposes Intel VMX to the Linux guest, so KVM inside WSL2 can create L2 VMs;
  `/dev/kvm` is present natively (`todo.md` §9, `CLAUDE.md`). baud's L2 Ubuntu boot therefore needs no
  bare-metal host.
- **Known L0 masks carry over**: Hyper-V masks `RDSEED`-exiting to L1 (MSR `0x48B` bit 48 = 0) while exposing
  `RDRAND`-exiting — so `rdseed` is handled by the build-time image rewrite (`todo.md` §3.8, §4.6), exactly as
  for any guest.
- **The determinism-critical dependency**: baud's work-clock is retired conditional branches read via
  `perf_event_open` **on the L1 (WSL2) vCPU thread**. Nested-PMU virtualization under Hyper-V is the one
  thing that must be validated before trusting the L2 fingerprint — `baud host probe` /
  `rcb_is_deterministic_on_this_cpu` (H0) gates it. If the L1 PMU is unavailable or nondeterministic, run the
  fingerprint on bare-metal Intel (the L2 boot itself still works under WSL2).

---

## 4. The Ubuntu guest image (`examples/ubuntu/spec.toml`)

- **Artifacts (18.04.1, frozen)**: the `cloud-images-archive.ubuntu.com` build whose `/etc/os-release`
  reads `PRETTY_NAME="Ubuntu 18.04.1 LTS"` — the qcow2 rootfs converted once to raw
  (`qemu-img convert -O raw …cloudimg-amd64.img rootfs.raw`), plus the stock `…-vmlinuz-generic` (4.15) and
  `…-initrd-generic`. Verify against the directory `SHA256SUMS`.
- **Boot**: `baud-boot` direct-boots the stock bzImage + stock initrd (the initrd carries `virtio_pci` /
  `virtio_blk`, which mount `/dev/vda1`). The machine must also provide, beyond the minimal-guest set: a
  **minimal ACPI** (RSDP → RSDT/XSDT → FADT + DSDT + MADT with one LAPIC), **PCI** (MCFG ECAM or legacy
  `0xCF8/0xCFC`), and the **deterministic virtio-blk** disk (§ below).
- **Deterministic disk**: one virtio-blk device backed by the **read-only, content-addressed** `rootfs.raw`
  plus an **in-memory copy-on-write overlay** for guest writes; block completion is delivered at a fixed
  work-clock boundary via the interrupt-injection engine (blkreplay-style), never on host-I/O return.
- **Command line** (4.15 GA kernel):

  ```
  systemd.unit=multi-user.target cloud-init=disabled console=ttyS0 root=/dev/vda1 ro rootwait
  nokaslr net.ifnames=0 biosdevname=0 clocksource=tsc tsc=reliable no_timer_check
  scsi_mod.scan=sync udev.children_max=1 fsck.mode=skip
  systemd.mask=systemd-timesyncd.service systemd.mask=systemd-time-wait-sync.service
  systemd.mask=systemd-random-seed.service systemd.mask=systemd-networkd.service
  systemd.mask=systemd-networkd-wait-online.service systemd.mask=systemd-resolved.service
  systemd.mask=cloud-init-local.service systemd.mask=cloud-init.service
  systemd.mask=cloud-config.service systemd.mask=cloud-final.service
  systemd.mask=apt-daily.timer systemd.mask=apt-daily-upgrade.timer systemd.mask=motd-news.timer
  systemd.mask=man-db.timer systemd.mask=fstrim.timer systemd.mask=systemd-tmpfiles-clean.timer
  systemd.mask=snapd.service systemd.mask=snapd.socket systemd.mask=snapd.seeded.service
  ```

- **One-time image prep** (hypervisor + image, not rootfs source edits): pin the NIC MAC (or emulate no NIC);
  emulate no RTC (guest starts at epoch 1970, deterministic); ship the rootfs **cleanly unmounted** (empty
  ext4 journal → zero replay writes); `tune2fs -c 0 -i 0 rootfs.raw` (disable mount-count / interval fsck).
- **The banner**: `Ubuntu 18.04.1 LTS ubuntu ttyS0` / `ubuntu login:` is `agetty` rendering `/etc/issue`
  (`\S` → `PRETTY_NAME`, `\n` → hostname `ubuntu`, `\l` → `ttyS0`). The exact three-token form needs
  `/etc/issue = \S \n \l`; the stock default `\S \l` omits the middle `ubuntu` — the one image line to
  confirm.

---

## 5. Determinism on the stock 4.15 kernel (no boot seed)

Ubuntu 18.04's 4.15 kernel **predates** `SETUP_RNG_SEED` (v6.0), `random.trust_bootloader` (v5.4), and
`random.trust_cpu` (v4.19), so `baud-boot`'s seed node is a **no-op** here (`baud-boot` omits it). Determinism
comes entirely from the machine pinning the CRNG's *inputs*:

- 4.15's `crng_initialize()` seeds the ChaCha20 key from the input pool, then **XOR-folds** (uncredited)
  `arch_get_random_seed_long()` (RDSEED) → `arch_get_random_long()` (RDRAND) → `random_get_entropy()` (RDTSC).
  baud makes all three constant (trapped RDTSC/RDRAND, rewritten RDSEED).
- The pool is *credited* only by `add_interrupt_randomness()` (RDTSC + jiffies + IRQ# + return-IP). baud's
  exact-boundary interrupt injection + deterministic TSC make that identical every boot, so `crng_init`
  reaches ready at the same instruction boundary.
- Zeroed RAM + `nokaslr` fix the pool's starting state and all addresses; the pinned MAC makes
  `add_device_randomness` a constant.

So `getrandom` / `/dev/urandom` are a pure function of the tape **on a kernel with no entropy-injection
support at all** — the strongest form of the `todo.md` §3.8 thesis. (On an 18.04 HWE 5.x kernel the seed
flags apply and drive the CRNG ready directly; `baud-boot` writes the seed node in that case.) Residual
sources to keep pinned: the exact interrupt-context register/IP capture and disk-completion timing (both
deterministic under baud's injection model).

---

## 6. The timed-exit fingerprint

At a fixed work-clock target `N`, baud stops the guest and dumps a four-field fingerprint. Implemented in
`baud-multiverse` (reused by the CLI); this section is the reference.

- **Stop at exactly `N` (arm-early-then-single-step, `todo.md` §3.4)**: arm the counter to overflow a margin
  before `N`, take the early exit, then single-step to the exact count. `deterministic events` = `N` =
  retired conditional branches (a raw `BR_INST_RETIRED.COND` perf event — *not* `HW_BRANCH_INSTRUCTIONS`,
  which counts all branches — pinned, `exclude_host`, identical on both VMs).

  ```rust
  ioctl(vcpu, KVM_SET_GUEST_DEBUG, &dbg{ control: KVM_GUESTDBG_ENABLE | KVM_GUESTDBG_SINGLESTEP });
  loop {
      let c: u64 = read(perf_fd);      // current retired conditional branches
      if c >= target { assert_eq!(c, target); break; }
      ioctl(vcpu, KVM_RUN, 0);         // exits KVM_EXIT_DEBUG after one instruction
  }
  ```

- **Guest RIP**: `KVM_GET_REGS` → `regs.rip` (guest-virtual / linear).
- **Guest-virtual → guest-physical**: `KVM_TRANSLATE` (`kvm_translation{ linear_address → physical_address,
  valid }`), cross-checked by a manual 4-level page walk from `CR3` (`KVM_GET_SREGS` → `cr3/cr4/efer`), which
  must agree:

  ```rust
  // long mode, 4-level; PFN = entry bits [51:12]; PS bit (7) ⇒ large page terminates early
  fn walk(cr3: u64, lin: u64, rd: impl Fn(u64,u64)->u64) -> Option<u64> {
      let mut t = cr3 & 0x000f_ffff_ffff_f000;
      for &(hi, lo, ps_mask) in &[(47,39,0u64),(38,30,0x000f_ffff_c000_0000),
                                   (29,21,0x000f_ffff_ffe0_0000),(20,12,0)] {
          let e = rd(t, (lin >> lo) & ((1<<(hi-lo+1))-1));
          if e & 1 == 0 { return None; }                       // not present
          if lo != 12 && e & (1<<7) != 0 {                     // large page
              return Some((e & ps_mask) | (lin & ((1<<lo)-1)));
          }
          t = e & 0x000f_ffff_ffff_f000;
      }
      Some(t | (lin & 0xfff))
  }
  ```

- **Guest memory hash**: `blake3` over the guest RAM slots in canonical order (by `guest_phys_addr`), a
  slot header (base+size) mixed in, **excluding** MMIO / device / host-written pages (the virtio-blk overlay
  is deterministic guest RAM and is included; any host-async page is excluded).

---

## 7. Cross-VM validation (`vm0` / `vm1`)

Two independent VMs — separate processes on separate cores (both L2 under one WSL2, or on two hosts) — boot
the identical `(image, tape)`, run to the same `N`, and dump the fingerprint. **Why they match byte-for-byte**:
the guest is a deterministic state machine (state = all RAM + all vCPU/MSR state; transition = one retired
instruction); the initial state is fixed by the image and every input is supplied by the tape; so "state at
branch `N`" is well-defined and identical, making `deterministic events`, `guest RIP`, `guest physical`, and
`guest memory hash` pure functions of `(image, tape)` — independent of host instance, core, or wall-clock.

Console output the drive asserts equal (field-for-field) between the two VMs:

```
Ubuntu 18.04.1 LTS ubuntu ttyS0

ubuntu login:
vm0 - timed exit:
deterministic events = <N>
guest RIP = <rip> (-> guest physical = <gpa>)
guest memory hash = <hash>
vm0: done
```
```
Ubuntu 18.04.1 LTS ubuntu ttyS0

ubuntu login:
vm1 - timed exit:
deterministic events = <same N>
guest RIP = <same rip> (-> guest physical = <same gpa>)
guest memory hash = <same hash>
vm1: done
```

What would break equality (each caught by the drive): a nondeterministic counter (unpinned / wrong event),
uninitialized RAM, a host-time / host-entropy leak, or an async host write into the hashed range.

---

## 8. Testing (H9 drive, `drive/h9.sh`)

```rust
#[test] fn ubuntu_boots_to_login() {
    let out = boot(ubuntu_1804_1(), tape());
    assert!(out.console.contains("Ubuntu 18.04.1 LTS ubuntu ttyS0")
         && out.console.trim_end().ends_with("ubuntu login:"));
}

#[test] fn timed_exit_fingerprint_is_stable() {
    let (a, b) = (run(ubuntu_1804_1(), tape.clone()), run(ubuntu_1804_1(), tape.clone()));
    assert_eq!(a.fingerprint_at(N), b.fingerprint_at(N));   // same VM, two runs
}

#[test] fn cross_vm_fingerprint_matches() {
    let vm0 = spawn_vm(ubuntu_1804_1(), tape.clone(), core = 2);
    let vm1 = spawn_vm(ubuntu_1804_1(), tape.clone(), core = 3);   // separate process + core
    let (f0, f1) = (vm0.timed_exit(N), vm1.timed_exit(N));
    assert_eq!((f0.events, f0.rip, f0.gpa, f0.mem_hash),
               (f1.events, f1.rip, f1.gpa, f1.mem_hash));
}
```

`drive/h9.sh` wraps it: `baud host probe` (assert `rcb_deterministic`) → build the Ubuntu image → boot `vm0`
and `vm1` on separate cores → timed exit at `N` → assert the four fields equal → print both reports.

---

## 9. Security Considerations

| Concern | Handling |
|---------|----------|
| Nested-virt trust | The L2 guest is confined by KVM inside the L1 WSL2 guest; baud adds no host device / DMA passthrough |
| PMU under nesting | The determinism proof depends on the L1 branch counter; H0 (`rcb_is_deterministic_on_this_cpu`) gates it; else run on bare metal |
| Distro treated as trusted | It is a guest under the machine — same mediation as any workload; no crate carries distro specifics |
| Fingerprint leaks host state | Hash excludes MMIO / host-written pages; all guest inputs are tape-served, not host-passthrough |

---

## 10. Future Considerations

| Feature | Description |
|---------|-------------|
| HWE 5.x / newer distros | Boot an 18.04 HWE or 22.04 kernel where `SETUP_RNG_SEED` / `trust_bootloader` apply directly |
| Two-host fingerprint | Run `vm0` and `vm1` on physically distinct hosts for the strongest cross-instance proof |
| Login + workload | Autologin and run a workload harness (`baud-guest-harness`) inside Ubuntu for an app-level determinism proof |
| Snapshot at login | Capture a universe at `ubuntu login:` and branch continuations from it |
