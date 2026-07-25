# Enforcement hook — concrete design (research complete, not yet built)

`baud_enforced_probe` (`BUILD.md`) only *reads* VMX capability MSRs. This document replaces the
prior speculation in `BUILD.md`'s "Not yet done" ("likely a `kvm_x86_ops`-level hook or an
eBPF/kprobe approach") with a design grounded in the actual kernel source already on this host
(`~/wsl-kernel-src/src`, the same tree `modules_prepare` already built against, kernel
`6.18.33.2-microsoft-standard-WSL2`). It was written *without* touching the live kernel — pure
source reading — deliberately, to de-risk the next iteration's build+load step before attempting
it.

## Why this cannot be an add-on module (kprobes are the wrong tool here)

The mechanism the enforced regime needs — a guest's raw `rdtsc`/`rdrand`/`rdseed` reaching
*userspace* instead of being resolved in-kernel — is controlled by two things, both private to
`arch/x86/kvm/vmx/vmx.c`, neither reachable from outside that file without recompiling it:

1. **The VM-execution control bits themselves.** `CPU_BASED_RDTSC_EXITING`
   (`arch/x86/include/asm/vmx.h:34`) and the secondary controls `SECONDARY_EXEC_RDRAND_EXITING`
   / `SECONDARY_EXEC_RDSEED_EXITING` (`vmx.h:67,72`) are cleared unconditionally today —
   `vmx.c:4443`: `exec_control &= ~(CPU_BASED_RDTSC_EXITING | ...)`. There is no ioctl (stock or
   otherwise) that lets userspace set these; they are computed once in `vmx_exec_control()`
   during vcpu setup from kernel policy, not from any per-vcpu userspace-supplied value.
2. **The exit-handler dispatch table.** `kvm_vmx_exit_handlers[]` (`vmx.c:6112`) is a `static`
   file-scope array of function pointers, indexed by exit reason. `static` means no other
   module — including one built the way `baud_enforced_probe` is — can see or overwrite it by
   symbol name; a kprobe can intercept a *function's entry*, but it cannot swap an entry out of
   a private array inside another compiled object. The only way to change what happens when
   `EXIT_REASON_RDTSC`/`RDRAND`/`RDSEED` fires is to change this file and rebuild the module it's
   compiled into — `kvm_intel.ko` itself, not a new sibling module.

So the spec's own phrase, "a small out-of-tree KVM patch/module" (todo.md §3.8), is accurate in
the literal sense: this is a *patch to* `kvm_intel.ko`'s source, rebuilt and loaded in place of
the stock one — not an independent add-on. `baud_enforced_probe` stays useful as the capability
pre-flight check; the actual hook is a different artifact (a patched `arch/x86/kvm/vmx/vmx.c`
built from the same `~/wsl-kernel-src/src` tree `BUILD.md` already set up).

## What the table lookup does today, confirmed from source — and why even a wrong patch fails safe

`vmx.c:6157-6158`:
```c
[EXIT_REASON_RDRAND] = kvm_handle_invalid_op,
[EXIT_REASON_RDSEED] = kvm_handle_invalid_op,
```
`kvm_handle_invalid_op` (`arch/x86/kvm/x86.c:2228`) just does `kvm_queue_exception(vcpu,
UD_VECTOR); return 1;` — it **resolves the exit entirely in-kernel** by injecting `#UD` into the
guest and resuming, the same outcome the cooperative regime already gets from the CPUID mask
(`rdrand_guest_is_flagged`, todo.md §3.2). **This means forcing `RDRAND`/`RDSEED`-exiting on by
itself changes nothing observable** — the handler table entry has to be replaced too, not just
the execution-control bit.

There is **no entry at all** for `EXIT_REASON_RDTSC` in the table. `__vmx_handle_exit`
(`vmx.c:6483`) bounds-checks the index (`vmx.c:6607-6608`) and null-checks the looked-up pointer
(`vmx.c:6628`) before calling it; either failure jumps to `unexpected_vmexit`
(`vmx.c:6633`), which sets `vcpu->run->exit_reason = KVM_EXIT_INTERNAL_ERROR` with
`KVM_INTERNAL_ERROR_UNEXPECTED_EXIT_REASON` and **returns cleanly to userspace** — it does not
panic, does not `BUG()`, does not touch other vcpus. This is the load-bearing fact that changes
the risk assessment from the prior iteration's "may want a second host to validate against
before investing in the hook": KVM's own exit dispatch is defensive by construction — a patch
that gets the table wiring wrong (forgets an entry, mistypes a reason number) degrades to a
guest-visible `KVM_EXIT_INTERNAL_ERROR` (which `baud-vcpu`'s catch-all already turns into
`Err(DeterminismHole)`, todo.md §3.6), not a host kernel panic. The bit this iteration did *not*
have time to source-verify is the execution-control-bit-set path itself (`vmx_exec_control`
correctness under a forced-on bit combined with existing nested-VMX / other quirks) — that is
the one part of this design that should be tested on real hardware before being trusted, per the
prior iteration's caution, but "test" here means "boot one guest and see," not "irreversibly
commit a kernel change."

## The patch, concretely

1. `vmx.c:4443` — do not clear `CPU_BASED_RDTSC_EXITING` (leave it set); same idea for
   `SECONDARY_EXEC_RDRAND_EXITING` in the equivalent secondary-exec-control computation. RDSEED
   is skipped: `baud_enforced_probe`'s own `dmesg` report already proved
   `SECONDARY_EXEC_RDSEED_EXITING` is **not settable on this host's VMX microcode**
   (`BUILD.md`'s "Result"), so this dev machine can only ever validate the RDTSC+RDRAND half of
   enforcement — spec'd, not a new limitation.
2. Add a new, small, self-contained handler, e.g. `handle_baud_deterministic_exit`, replacing the
   `kvm_handle_invalid_op` entries for `RDRAND`/`RDSEED` and filling the previously-empty `RDTSC`
   slot. It does not emulate anything itself — it fills a payload (which instruction; for RDRAND,
   which GPR the result goes to, decoded from `vmx_get_exit_qual`/instruction info the same way
   `handle_cr`/`handle_io` already do for their own operands) and returns `0` so KVM's run loop
   exits to userspace, exactly the existing `IoIn`/`MmioRead`-style contract `baud-vcpu` already
   consumes.
3. A new `KVM_EXIT_*` constant in `include/uapi/linux/kvm.h`. The highest defined today on this
   tree is `KVM_EXIT_TDX = 40` (`kvm.h:186`); `KVM_EXIT_BAUD_DETERMINISM = 41` is free. A payload
   struct in the `kvm_run` exit-reason union (mirroring `kvm_run.rdmsr`/`wrmsr`'s existing
   `X86_RDMSR`/`X86_WRMSR` shape) carries: instruction kind (RDTSC=0/RDRAND=1), destination GPR
   index for RDRAND.

## The userspace side needs zero changes to any pinned crate

This was the open scoping question worth answering before committing to the kernel-side work:
does a new, made-up `KVM_EXIT_*` number require patching pinned `kvm-ioctls` 0.25 too? **No.**
`kvm-ioctls-0.25.0/src/ioctls/vcpu.rs:1658` already has a catch-all: any `exit_reason` value
`run()` doesn't recognize returns `Ok(VcpuExit::Unsupported(r))` — `KVM_EXIT_BAUD_DETERMINISM`
would surface exactly this way, no crate fork needed. For the payload beyond the bare reason
number, `Vcpu::get_kvm_run(&mut self) -> &mut kvm_run` (`vcpu.rs:1689`) already exposes the raw,
mmap'd `kvm_run` struct — `kvm-bindings` 0.14's generated union type won't have a named field for
a payload struct invented after that crate was bindgen'd, but the union's existing
byte-generic member (bindgen always emits one for the largest-variant padding) is enough for
`baud-vcpu` to read/write the new struct's bytes via an `unsafe` pointer cast, the same category
of raw-struct-poking `baud-vcpu::linux::pmu` already does for `F_SETSIG` (see that module's own
precedent, noted in `specs/baud-snapshot.md`'s dirty-ring writeup as "derive don't hand-encode").
`baud-vcpu`'s `dispatch_exit` gains one new match arm on `Exit::Unsupported(BAUD_EXIT_DETERMINISM)`
alongside the existing named exits — a normal, additive change to that crate's own (not pinned)
code, not a dependency-version problem.

## What is still open after this design

- The design above is unbuilt and untested — no line of `vmx.c` has been changed, no
  `kvm_intel.ko` rebuilt, no boot attempted with it. That is the next concrete step, now scoped
  down from "materially larger, separate task" to a bounded patch against three known source
  locations plus one new userspace match arm.
- RDSEED-exiting stays permanently out of reach on this exact host (hardware, not code) —
  whoever builds this next should treat that as expected, not a regression, and the resulting
  regime is "RDTSC+RDRAND enforced, RDSEED still CPUID-masked" until validated on hardware that
  does expose `SECONDARY_EXEC_RDSEED_EXITING`.
- `crates/baud-host/src/linux.rs`'s `enforced_module_present()` should keep returning `false`
  until a built, loaded, *and* boot-tested module exists — this document is a plan, not evidence
  of a working regime (`regime_is_recorded_and_not_overclaimed`).
