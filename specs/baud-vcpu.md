<!--
 Copyright (c) 2026 Henrique Falconer. All rights reserved.
 SPDX-License-Identifier: Proprietary
-->

# Baud Vcpu Specification

**Status:** Planned\
**Version:** 1.0\
**Last Updated:** 2026-07-24

---

## 1. Overview

### Purpose

`baud-vcpu` is the single-virtual-CPU state machine and VM-exit dispatch. It owns the `KVM_RUN` loop for one
vCPU and routes every exit to a deterministic handler. It is the inner loop of `baud-multiverse`; keeping it
a separate crate makes the exit dispatch testable in isolation.

### Goals

- **One instruction stream**: exactly one vCPU per VM, so an execution point is nameable by a tuple
- **Every exit deterministic**: each `VcpuExit` resolves to a computed value; the catch-all fails loud
- **Boundary control**: single-step and interrupt-inject at an exact instruction boundary

### Non-Goals

- More than one vCPU (multi-core guest determinism is out of scope)
- Real device emulation (served by `baud-tape-device` and the console)

---

## 2. Crate Architecture

```
┌──────────────────────────────────────────────┐
│                   baud-vcpu                    │
│  KVM_RUN loop · VcpuExit dispatch              │
│  single-step + interrupt injection engine      │
└───────────────────┬──────────────────────────┘
                    │ uses
      kvm-ioctls · kvm-bindings · vmm-sys-util
```

### Rationale

- Deps = `{kvm-ioctls 0.25, kvm-bindings 0.14, vmm-sys-util 0.15, baud-proto}`; no `tokio` (one thread).
- Knows CPU exits, not workloads. Soft budget ≤ 2,000 LOC.

---

## 3. Exit Dispatch

Every exit resolves deterministically; the catch-all returns `Err(DeterminismHole)`.

```rust
loop {
    match self.vcpu.run()? {
        VcpuExit::IoIn(port, data)     => self.bus.pio_read(port, data)?,   // tape/console
        VcpuExit::IoOut(port, data)    => self.bus.pio_write(port, data)?,
        VcpuExit::MmioRead(gpa, data)  => self.bus.mmio_read(gpa, data)?,
        VcpuExit::MmioWrite(gpa, data) => self.bus.mmio_write(gpa, data)?,
        VcpuExit::X86Rdmsr(_)          => self.time.serve_rdmsr(&mut self.vcpu)?, // virtual TSC/AUX/deadline
        VcpuExit::X86Wrmsr(_)          => self.time.absorb_wrmsr(&mut self.vcpu)?,
        VcpuExit::Hlt | VcpuExit::Shutdown => return Ok(Halted),
        VcpuExit::Debug(_)             => return Ok(SingleStep), // boundary walk (see §5)
        other => return Err(DeterminismHole(other)),            // never continue
    }
}
```

Open-bus PIO/MMIO reads return a fixed byte (`0xFF`), never host memory.

---

## 4. Single vCPU Rule

| Rule | Enforcement |
| ------------------------------ | ------------------------------------------ |
| Exactly one vCPU per VM        | `Vm::create` rejects `n_vcpus != 1` |
| Thread pinned to one core      | `sched_setaffinity` at start; CPUID topology pinned |
| No host interrupts into guest  | Only VMM-scheduled events are injected |

---

## 5. Interrupt Injection at an Exact Boundary

Arm-early-then-single-step to a target work-count (retired conditional branches):

```rust
fn inject_at(&mut self, target_rcb: u64, vector: u8) -> Result<()> {
    self.pmu.arm_overflow(target_rcb - MARGIN);          // 1. arm early
    self.run_until_exit()?;                               // 2. sloppy early exit
    self.set_guest_debug(SINGLESTEP | BLOCKIRQ)?;         // 3. step, no stray IRQ
    while !self.at_point(target_rcb) { self.step()?; }    //    match (PC + regs + RCB [+RCX/+stack])
    if !self.ready_for_interrupt_injection() {            // 4. ensure injectable
        self.request_interrupt_window()?; self.run_until_irq_window()?;
    }
    self.inject(vector)                                   // 5. KVM_INTERRUPT / SET_VCPU_EVENTS
}
```

`at_point` compares `(rip, all GP regs, rcb)`, adds `rcx` for `rep` loops and a stack checksum on collision.

---

## 6. Testing

```rust
#[test] fn vm_creation_refuses_multiple_vcpus() {
    assert!(Vm::create(VmCfg { n_vcpus: 2, .. }).is_err());
}

#[test] fn no_unmodeled_exit_is_silent() {
    // fuzz random tapes; the loop never leaves the match without Ok/Err
    for tape in random_tapes(1000) {
        assert!(matches!(run_loop(tape), Ok(_) | Err(DeterminismHole(_))));
    }
}

#[test] fn timer_tick_lands_at_identical_instruction() {
    let a = run(timer_guest(), tape.clone()).injection_tuples();
    let b = run(timer_guest(), tape).injection_tuples();
    assert_eq!(a, b); // (PC + RCB) identical at every tick
}
```

---

## 7. Security Considerations

| Threat | Handling |
| ------------------------------ | ------------------------------------------ |
| Guest escapes the vCPU         | KVM confines the guest to its memory slots; no host device access |
| Nondeterminism leaks via an exit | Catch-all fails loud; open-bus reads are fixed |
| Interrupt injected off-boundary | Injection gated on the exact `(PC + regs + RCB)` tuple |

---

## 8. Future Considerations

| Feature | Description |
| ------------------ | ---------------------------------------------- |
| Enforced regime hooks | Consume RDTSC/random-instruction exits when the custom KVM module is present |
| AMD dispatch       | VMCB-based intercepts and TSC-ratio scaling (phase-2) |
