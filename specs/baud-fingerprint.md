<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Fingerprint Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-25

---

## 1. Overview

### Purpose

`baud-fingerprint` implements the **timed-exit determinism check**: it stops a running guest at a fixed
work-clock point, captures a four-field fingerprint (`deterministic events`, `guest RIP` → `guest physical`,
`guest memory hash`), renders the exact console report, and — across two independent VMs (`vm0`, `vm1`) on the
same `(image, tape)` — asserts the fingerprints are byte-identical. It is how baud *proves*, not merely
claims, that a whole-machine run (e.g. the Ubuntu 18.04.1 boot, `specs/baud-ubuntu.md`) is a pure function of
`(image, tape)`. The capture primitives live in `baud-multiverse`; the report format, comparator, and
two-VM orchestration are specified here and surfaced as `baud verify fingerprint` in `baud-cli` +
`drive/h9.sh`.

### Goals

- **A canonical report**: one exact, stable text block per VM (banner + four fields + `done`).
- **A structured comparator**: compare the four fields field-by-field (not fragile text), reporting the
  first divergence.
- **Two-VM orchestration**: boot `vm0`/`vm1` as separate processes on separate cores, same tape, and pass/fail
  on equality.
- **Determinism about the check itself**: the report is a fixed function of `(image, tape, N)`; the check has
  no wall-clock or host-state input.

### Non-Goals

- Producing the fingerprint values (the KVM ioctls / page walk / hash live in `baud-multiverse`,
  `specs/baud-ubuntu.md` §6).
- Guaranteeing the branch counter is deterministic on a given host — that is H0's job
  (`rcb_is_deterministic_on_this_cpu`); this crate consumes a validated counter.

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────────────────┐
│                    baud-fingerprint                        │
│  Fingerprint capture hook (via baud-multiverse)            │
│  Report renderer (exact 6-line block)                      │
│  Cross-VM comparator (field-by-field, first-divergence)    │
└───────────────────┬──────────────────────────────────────┘
     ▲ two VMs (vm0,vm1) from baud-server, same (image,tape)
     │ surfaced as `baud verify fingerprint` + drive/h9.sh
```

### Rationale

- Deps = `{baud-multiverse (capture), baud-proto (wire types), blake3}`. Soft budget ≤ 600 LOC. Pure logic
  over a captured `Fingerprint`; no KVM code of its own — it calls `Multiverse::timed_exit`.

### Types & API

```rust
pub struct Fingerprint {
    pub label: String,       // "vm0" / "vm1"
    pub banner: Vec<u8>,     // the serial console tail at the stop (login banner)
    pub events: u64,         // deterministic events = retired conditional branches (= N)
    pub rip: u64,            // guest-virtual RIP
    pub gpa: Option<u64>,    // guest-physical of RIP; None if unmapped
    pub mem_hash: [u8; 32],  // blake3 over guest RAM slots (canonical, MMIO excluded)
}

impl Fingerprint {
    /// Stop the guest at exactly `n` deterministic events and capture the four fields + banner.
    pub fn capture(vm: &mut Multiverse, label: &str, n: u64) -> Result<Self, FpError>;
    /// Render the exact console report block (§3).
    pub fn render(&self) -> String;
}

/// Compare two fingerprints field-by-field; Ok(()) if identical, Err names the first divergent field.
pub fn compare(a: &Fingerprint, b: &Fingerprint) -> Result<(), Divergence>;
```

---

## 3. The report format (exact, byte-stable)

The report is the captured **login banner** followed by five fingerprint lines and a `done` line. For the
Ubuntu target (`specs/baud-ubuntu.md`) the banner is `Ubuntu 18.04.1 LTS ubuntu ttyS0` + a blank line +
`ubuntu login:`; a non-distro guest supplies its own banner (or an empty one). Exact block for `vm0`:

```
Ubuntu 18.04.1 LTS ubuntu ttyS0

ubuntu login:
vm0 - timed exit:
deterministic events = <N>
guest RIP = <rip> (-> guest physical = <gpa>)
guest memory hash = <hash>
vm0: done
```

Field rendering rules (so text comparison and eyeballing are unambiguous):

| Field | Rule |
|-------|------|
| banner | the raw serial-console tail bytes at the stop, verbatim |
| `<N>` | `events` in **decimal**, no separators |
| `<rip>` | `format!("0x{:016x}", rip)` — 16 lowercase hex digits |
| `<gpa>` | `format!("0x{:016x}", gpa)`, or the literal `unmapped` if `gpa == None` |
| `<hash>` | lowercase hex of the 32-byte blake3 digest (64 chars) |
| `<label>` | `"vm0"` / `"vm1"` (the only per-VM-varying token by design) |

```rust
fn render(&self) -> String {
    let gpa = self.gpa.map(|g| format!("0x{:016x}", g)).unwrap_or_else(|| "unmapped".into());
    format!(
        "{banner}\n{l} - timed exit:\n\
         deterministic events = {n}\n\
         guest RIP = 0x{rip:016x} (-> guest physical = {gpa})\n\
         guest memory hash = {hash}\n\
         {l}: done\n",
        banner = String::from_utf8_lossy(&self.banner),
        l = self.label, n = self.events, rip = self.rip, gpa = gpa,
        hash = hex::encode(self.mem_hash),
    )
}
```

Because only `<label>` differs by design, two matching VMs produce blocks that are identical after
substituting `vm0`↔`vm1` — which is exactly what §6 checks.

---

## 4. Capturing a fingerprint

`Fingerprint::capture` drives the `baud-multiverse` timed-exit primitive (full detail in
`specs/baud-ubuntu.md` §6):

1. **Stop at exactly `N`** via arm-early-then-single-step (`todo.md` §3.4): arm the retired-conditional-branch
   counter (a pinned raw `BR_INST_RETIRED.COND` perf event, `exclude_host`) to overflow a margin before `N`,
   take the early exit, then `KVM_SET_GUEST_DEBUG(SINGLESTEP)` until `read(perf_fd) == N`. `events = N`.
2. **`guest RIP`**: `KVM_GET_REGS` → `regs.rip`.
3. **`guest physical`**: `KVM_TRANSLATE` (cross-checked by the manual CR3 4-level page walk in
   `baud-ubuntu` §6); `None` if `valid == 0`.
4. **`guest memory hash`**: blake3 over the RAM slots in canonical order (by `guest_phys_addr`, with a
   `(base,size)` header per slot), excluding MMIO / host-written pages.
5. **`banner`**: the last `K` bytes emitted to the serial console up to the stop (§5).

```rust
fn capture(vm: &mut Multiverse, label: &str, n: u64) -> Result<Fingerprint, FpError> {
    vm.run_to_events(n)?;                       // arm-early-then-single-step to exactly n
    let regs  = vm.get_regs()?;
    let gpa   = vm.translate(regs.rip)?;        // KVM_TRANSLATE + page-walk cross-check
    let hash  = vm.hash_ram_canonical();        // blake3 over RAM slots, MMIO excluded
    Ok(Fingerprint { label: label.into(), banner: vm.console_tail(K),
                     events: n, rip: regs.rip, gpa, mem_hash: hash })
}
```

---

## 5. Detecting "reached login" (deterministically)

The banner in the report doubles as proof the guest reached the login prompt — and it does so at a **fixed
work-clock**, not a wall-clock timeout:

- The VMM watches the emulated UART (`ttyS0`) TX byte stream. Because emission is instruction-count
  deterministic, the byte sequence `…Ubuntu 18.04.1 LTS ubuntu ttyS0\n\nubuntu login: ` appears at the **same
  deterministic-events offset every run**.
- Two equivalent checks: (a) match the banner byte sequence and record the event offset — assert it equals the
  recorded value; or (b) run to the agreed `N` (chosen ≥ the login offset) and assert `console_tail(K)` ends
  with the expected banner. baud uses (b): `N` is fixed, and `capture` asserts the tail equals the banner, so
  a run that *didn't* reach login fails the capture rather than silently fingerprinting the wrong state.

```rust
const UBUNTU_BANNER: &str = "Ubuntu 18.04.1 LTS ubuntu ttyS0\n\nubuntu login: ";
assert!(fp.banner.ends_with(UBUNTU_BANNER.as_bytes()), "did not reach login by event {N}");
```

---

## 6. The cross-VM comparator

Compare the **structured** fingerprints, not the rendered text (so a formatting change can't mask a real
divergence, and the first differing field is named):

```rust
pub fn compare(a: &Fingerprint, b: &Fingerprint) -> Result<(), Divergence> {
    if a.banner   != b.banner   { return Err(Divergence::field("banner")); }
    if a.events   != b.events   { return Err(Divergence::field("deterministic events")); }
    if a.rip      != b.rip      { return Err(Divergence::field("guest RIP")); }
    if a.gpa      != b.gpa      { return Err(Divergence::field("guest physical")); }
    if a.mem_hash != b.mem_hash { return Err(Divergence::field("guest memory hash")); }
    Ok(())                       // label is intentionally NOT compared
}
```

- **Equality of all four fields + the banner** ⇒ the whole-machine state at `N` is identical across the two
  VMs ⇒ the run is a pure function of `(image, tape)` (`specs/baud-ubuntu.md` §7).
- **On divergence**, the named field localizes the leak: `guest memory hash` alone ⇒ a RAM byte differs (often
  uninitialized memory or a host-written page in the hashed range); `guest RIP`/`events` differ ⇒ the
  instruction streams diverged (a nondeterministic counter, a host-time/entropy leak, or a nondeterministic
  interrupt); `guest physical` differs with equal RIP ⇒ page tables diverged.
- **Exit code**: `0` if `compare` is `Ok`; `2` (goal/violation, `todo.md` §1) if divergent — a determinism
  violation, reported with the field and both values.

---

## 7. Orchestration (`baud verify fingerprint` / `drive/h9.sh`)

```rust
// baud verify fingerprint --image ubuntu-18.04.1 --tape T --events N
let tape = load(T);
let mut vm0 = server.spawn(image, tape.clone(), Placement::core(2));   // separate process
let mut vm1 = server.spawn(image, tape.clone(), Placement::core(3));   // separate core
let f0 = Fingerprint::capture(&mut vm0, "vm0", N)?;
let f1 = Fingerprint::capture(&mut vm1, "vm1", N)?;
print!("{}", f0.render());
print!("{}", f1.render());
match compare(&f0, &f1) {
    Ok(())      => { eprintln!("determinism: OK"); exit(0); }
    Err(d)      => { eprintln!("determinism VIOLATED at {}: {} != {}", d.field, d.a, d.b); exit(2); }
}
```

`drive/h9.sh` wraps it: `baud host probe` (assert `rcb_deterministic`, else skip with a recorded reason) →
`baud image build examples/ubuntu` → `baud verify fingerprint --events N` → assert exit `0` and that both
rendered blocks carry the Ubuntu banner. The two VMs may be two L2 guests under one WSL2 host (separate
processes/cores) or on two hosts (`specs/baud-ubuntu.md` §3, §10). Before trusting a nested host, run the
fast userspace PMU pre-check `tools/pmucheck.c` (retired-branch counter availability + determinism); the
authoritative guest-level gate remains H0's `rcb_is_deterministic_on_this_cpu`.

---

## 8. Testing

```rust
#[test] fn render_is_byte_exact() {
    let fp = Fingerprint { label: "vm0".into(), banner: UBUNTU_BANNER.into(),
        events: 4_812_337, rip: 0xffffffff81abc123, gpa: Some(0x0000_0001_abc123),
        mem_hash: [0x11; 32] };
    assert_eq!(fp.render(),
        "Ubuntu 18.04.1 LTS ubuntu ttyS0\n\nubuntu login: \nvm0 - timed exit:\n\
         deterministic events = 4812337\n\
         guest RIP = 0xffffffff81abc123 (-> guest physical = 0x00000001abc123)\n\
         guest memory hash = 1111111111111111111111111111111111111111111111111111111111111111\n\
         vm0: done\n");
}

#[test] fn compare_reports_first_divergence() {
    let (a, mut b) = (fp("vm0"), fp("vm1"));
    b.mem_hash[0] ^= 1;
    assert_eq!(compare(&a, &b).unwrap_err().field, "guest memory hash");
}

#[test] fn label_difference_is_not_a_divergence() {
    assert!(compare(&fp("vm0"), &fp("vm1")).is_ok());   // only labels differ
}

#[test] fn missing_login_fails_capture() {
    let mut vm = boot(ubuntu(), tape());
    assert!(matches!(Fingerprint::capture(&mut vm, "vm0", TOO_EARLY), Err(FpError::NoBanner)));
}
```

`cross_vm_fingerprint_matches` (H9) is the integration test: two real VMs, `compare` returns `Ok`.

---

## 9. Security Considerations

| Concern | Handling |
|---------|----------|
| A false "identical" (both wrong the same way) | The banner assert proves each VM actually reached login; the hash covers all guest RAM |
| Hash leaks host state | Canonical RAM slots only; MMIO / host-written / pvclock pages excluded from the digest |
| Comparator masks a divergence | Structured field compare (not text); label deliberately excluded; first-divergence named |
| Counter not trustworthy | Use the raw `BR_INST_RETIRED.COND` event (the all-branch `HW_BRANCH_INSTRUCTIONS` is measured ±1, `docs/determinism.md`); gated on H0 `rcb_is_deterministic_on_this_cpu`; a nondeterministic counter is refused, not fudged |

---

## 10. Future Considerations

| Feature | Description |
|---------|-------------|
| N-way check | Compare M VMs (a quorum) instead of two, for a stronger cross-instance proof |
| Fingerprint ladder | Capture at several `N` values (boot, login, post-login) and compare each |
| Signed fingerprints | Attach a `baud-identity` signature so a fingerprint is attributable/auditable |
| Divergence bisect | On mismatch, binary-search `N` to the first divergent event for root-cause |
