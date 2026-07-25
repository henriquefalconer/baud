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
   is skipped here: `baud_enforced_probe`'s own `dmesg` report already proved
   `SECONDARY_EXEC_RDSEED_EXITING` is **not settable on this host's VMX microcode**
   (`BUILD.md`'s "Result"). *This was originally recorded as a permanent ceiling on what this dev
   machine could validate; it is not — see "RDSEED, without any secondary control at all" below.*
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

## RDSEED, without any secondary control at all (`ud2-enforce.patch`)

The two bullets above, and everything in `BUILD.md`'s "Result", assumed RDSEED enforcement meant
*trapping the `RDSEED` instruction* — which this host's microcode makes impossible. **That
assumption is what was wrong, not the hardware finding.** `baud-packages`
(`crates/baud-packages/src/rdseed.rs`, todo.md §4) rewrites every `rdseed` opcode in a guest ELF's
executable sections to `UD2` (`0F 0B`) + `NOP` padding at **build** time, in place and
length-preserving. The real `RDSEED` opcode therefore never executes in the guest at all, and
`SECONDARY_EXEC_RDSEED_EXITING` is moot for this path — a host that cannot set that bit (this one)
enforces RDSEED exactly as well as one that can.

What makes this cheap on the kernel side is a second finding, from reading `vmx.c` rather than
guessing: **`#UD` already causes a VM-exit with no patch whatsoever.**
`vmx_update_exception_bitmap` (`vmx.c:~819`) unconditionally includes `UD_VECTOR` in the exception
bitmap, because stock KVM wants the trap for its own software-instruction-emulation fallback. Every
guest `#UD` already takes exactly one path: `EXIT_REASON_EXCEPTION_NMI` → `handle_exception_nmi` →
`is_invalid_opcode(intr_info)` (`vmx.c:~5212`) → `handle_ud` (`x86.c:~8054`). So `ud2-enforce.patch`
adds **no exec-control change and no exception-bitmap change at all** — the earlier assumption that
it would need one (and that `kvm-bindings` 0.14.1's missing `exception_bitmap` field was therefore a
problem) is simply not applicable. It intercepts that one branch with `handle_baud_ud2_exit`, which:

1. Reads the 2 bytes at `kvm_get_linear_rip(vcpu)` via `kvm_read_guest_virt` — the same read
   `handle_ud` itself already does for its force-emulation-prefix signature check.
2. If the read fails, or the bytes are not exactly `0F 0B`, tail-calls `handle_ud(vcpu)` unchanged.
   This is load-bearing: the Linux kernel's own `BUG()`/`WARN_ON()` compile to a bare `UD2`, and
   every genuinely invalid opcode raises the same `#UD`, so anything not positively identified must
   keep behaving exactly as it does with no patch loaded.
3. Otherwise sets `KVM_EXIT_BAUD_DETERMINISM` with payload low byte `2` and returns 0.

Unlike the RDTSC/RDRAND handlers, it **never calls `kvm_skip_emulated_instruction`**: RIP stays
exactly at the trapping `UD2`. Only userspace's image-specific site table knows how far a
*confirmed* site's `UD2`+`NOP` padding extends (3 bytes for `RDSEED r32`, 4 for `r64`) and which
GPR the original instruction targeted — the `UD2` that replaced it encodes neither — so userspace
advances RIP itself on a hit, and re-injects `#UD` at that same untouched RIP
(`KVM_SET_VCPU_EVENTS`) on a miss, which is what a native un-intercepted fault would have reported.

The consequence for the regime as a whole: **RDTSC + RDRAND + RDSEED are all enforceable on this
exact dev host**, not just the first two. `drive/h3-enforced-rdseed.sh` exercises both halves
(served site, and re-injected non-site) against real `/dev/kvm`.

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

*(This section was written when nothing here was built. Struck through where since superseded.)*

- ~~The design above is unbuilt and untested~~ — all three patches now exist, build against
  `~/wsl-kernel-src/src`, and are hardware-tested: `rdtsc-enforce.patch` and `rdrand-enforce.patch`
  via `drive/h3-enforced-rdtsc.sh`/`drive/h3-enforced-rdrand.sh`, and `ud2-enforce.patch` via
  `drive/h3-enforced-rdseed.sh` — both its served-site and re-injected-non-site halves pass against
  real `/dev/kvm` with the patched module swapped in, and the stock module restores cleanly on exit.
- ~~RDSEED-exiting stays permanently out of reach on this exact host~~ — true of the *secondary
  control*, irrelevant to the *regime*: see "RDSEED, without any secondary control at all" above.
  The resulting regime is "RDTSC + RDRAND + RDSEED all enforced", on this host, with no hardware
  caveat left.
- `crates/baud-host/src/linux.rs`'s `enforced_module_present()` still returns `false` — but now for
  a different reason than "no such module exists" (see that function's own doc): the patched module
  is only ever swapped in transiently by the `drive/h3-enforced-*.sh` scripts, which always restore
  the stock one, so no ordinary process on this host is running under it. Wiring this to a real
  runtime check (a `KVM_CHECK_EXTENSION` the patches would have to add) is the outstanding work.
- Nothing plumbs a real `baud image build`'s `RdseedRewriteReport` into
  `Multiverse::boot_with_rdseed_sites` yet — the one enforced-RDSEED test hardcodes the
  hand-verified site of a fixed fixture image
  (`crates/baud-multiverse/tests/fixtures/rdseed-guest/BUILD.md`). An explicit scope cut, not a gap
  in the serve-path mechanism.
