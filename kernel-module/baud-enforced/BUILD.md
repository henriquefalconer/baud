# baud_enforced_probe — build/load notes

This is the first real, on-hardware artifact toward the "enforced" determinism regime
(todo.md §3.8, specs/baud-multiverse.md §3.8, specs/baud-host.md §8). It does not implement
enforcement; it only answers the open hardware question of whether this host's VMX
microcode allows the VM-execution controls the enforced regime needs (RDTSC-exiting,
RDRAND/RDSEED-exiting) to be turned on at all. It never writes VMX state and never touches
a running vCPU — read-only capability-MSR probing only.

**Both build and `insmod` are verified working on this dev host** (see "Result" below).

## Building and loading

The stock WSL2 kernel (`uname -r` = `6.18.33.2-microsoft-standard-WSL2` on this dev
machine) ships no `linux-headers-*` package and no `/lib/modules/$(uname -r)/build` —
out-of-tree modules cannot be built against it out of the box. To build and load one:

```
sudo apt-get install -y dwarves   # pahole — MUST be installed BEFORE the olddefconfig
                                   # step below; see "Known blocker" for why the order matters

mkdir -p ~/wsl-kernel-src && cd ~/wsl-kernel-src
git clone --depth 1 --branch linux-msft-wsl-6.18.33.2 \
    https://github.com/microsoft/WSL2-Linux-Kernel.git src
cd src
rm -rf .git   # a shallow clone defeats scripts/setlocalversion's tag lookup (Microsoft's
              # tags aren't named like mainline vX.Y.Z, so `git describe` never resolves
              # cleanly) and it always appends a spurious "+" to kernelrelease, which then
              # fails vermagic matching at insmod time. This tree is a one-off scratch
              # build tree, not something that needs its own version control.
zcat /proc/config.gz > .config      # the exact config this running kernel was built with
make CC=gcc-13 olddefconfig         # gcc-13 just to match CONFIG_CC_HAS_COUNTED_BY's major
                                     # version gate — the exact vendor patch version does NOT
                                     # matter, see "Known blocker" below
make CC=gcc-13 modules_prepare -j$(nproc)
sudo ln -sfn "$PWD" "/lib/modules/$(uname -r)/build"

cd /path/to/baud/kernel-module/baud-enforced
KBUILD_MODPOST_WARN=1 make CC=gcc-13   # see below for why KBUILD_MODPOST_WARN is needed
sudo insmod baud_enforced_probe.ko
sudo dmesg | tail -10                  # the capability report
sudo rmmod baud_enforced_probe         # read-only probe, safe to unload any time
```

`KBUILD_MODPOST_WARN=1` is needed because `modules_prepare` alone (no full `vmlinux`
build) never produces `Module.symvers`, so modpost can't resolve ordinary exported kernel
symbols like `printk` at build time — it still resolves correctly at `insmod` time against
the real running kernel's symbol table. This is standard for headers-only WSL2 module
builds, not specific to this module.

## Known blocker (now fixed): insmod failed with a struct-module-size mismatch

Earlier attempts hit, on `insmod baud_enforced_probe.ko`:

```
module baud_enforced_probe: .gnu.linkonce.this_module section size must match the
kernel's built struct module size at run time
```

This is `kernel/module/main.c`'s `elf_validity_cache_index_mod()` rejecting a `sizeof(struct
module)` mismatch between the module and the running kernel.

**Two hypotheses were tried and ruled out before finding the real cause.** First, a
`CONFIG_CC_HAS_COUNTED_BY`-style codegen divergence from using the system's `gcc-15`/`gcc-
13.4.0` instead of the exact vendor build (`gcc (GCC) 13.2.0` + `GNU ld 2.41`, per
`/proc/version`) was suspected. To test this rigorously, the *exact* `gcc-13.2.0-4ubuntu3`
and `binutils_2.41-5ubuntu1` `.deb`s were fetched from `old-releases.ubuntu.com` and
extracted into a local prefix (no system install) to build with a byte-for-byte matching
compiler+assembler+linker. **The mismatch persisted even then** — this definitively rules
out any toolchain-version explanation; `CC=gcc-13` (the system package, any recent 13.x) is
sufficient and the old-toolchain dance is unnecessary.

**The real cause**: `pahole` (the `dwarves` package) was not installed on this machine when
`make olddefconfig` was first run. `CONFIG_DEBUG_INFO_BTF_MODULES` (and `CONFIG_DEBUG_INFO_BTF`)
depend on Kconfig's `$(success,...)` probe finding a working `pahole` at *config-generation*
time — with no `pahole` present, `CONFIG_PAHOLE_VERSION` silently resolves to `0` and both
options silently drop out of `.config`, even though the real running kernel's own
`/proc/config.gz` has both set to `y` (`CONFIG_PAHOLE_VERSION=125`). `CONFIG_DEBUG_INFO_BTF_MODULES`
gates four extra fields on `struct module` (`btf_data_size`/`btf_base_data_size`/`btf_data`/
`btf_base_data`, `include/linux/module.h`) — exactly 24 bytes / 4 members, confirmed via
`pahole -C module /sys/kernel/btf/vmlinux` (real: 1280 bytes / 71 members) vs. `pahole -C
module baud_enforced_probe.o` on a `-g` build (broken: 1216 bytes / 67 members) before the
fix. Installing `pahole` *before* `zcat /proc/config.gz > .config && make olddefconfig`
lets `CONFIG_DEBUG_INFO_BTF_MODULES=y` survive `olddefconfig` intact, and the size matches.

**Lesson for any future from-`/proc/config.gz` out-of-tree kernel module build on this
machine**: install `dwarves` (`pahole`) first. A missing build-time tool that silently
downgrades a `.config` option is a much more common source of ABI drift than a compiler
patch-version mismatch, and is worth checking before chasing toolchain reproduction.

## Result

`insmod baud_enforced_probe.ko` succeeds (exit 0), the module loads and appears in `lsmod`,
and its real capability report reaches `dmesg`. On this dev host (Intel, WSL2/Hyper-V
nested virtualization):

```
VMX capability report for the enforced regime (todo.md "enforced-regime KVM module"):
  RDTSC-exiting settable:          yes
  secondary controls available:    yes
  RDRAND-exiting settable:         yes
  RDSEED-exiting settable:         NO
  enforced regime hardware-feasible on this CPU: no
```

This is a genuine, previously-unknown hardware finding, not a bug in the probe: this
specific host's VMX microcode does not expose the RDSEED-exiting secondary control.

**The conclusion originally drawn from it — "the enforced regime cannot be hardware-feasible
here regardless of what module code is written" — was wrong, and is superseded.** It assumed
enforcing RDSEED meant trapping the `RDSEED` *instruction*. It does not: `baud-packages`
(`crates/baud-packages/src/rdseed.rs`, todo.md §4) rewrites every `rdseed` opcode to `UD2` +
`NOP` padding at build time, so the real opcode never executes in the guest and this
secondary control is moot for that path. The resulting `UD2`'s `#UD` is already trapped by
the exception bitmap stock KVM sets unconditionally, and `ud2-enforce.patch` serves it — see
`ENFORCEMENT_DESIGN.md`'s "RDSEED, without any secondary control at all". The line above
stays as recorded because the *measurement* is still true and still worth knowing; only the
inference from it changed. This dev machine can validate a fully-enforced run.

**Not yet done**: implementing the actual enforcement (hooking KVM's own VMCS setup to
force the RDTSC/RDRAND/RDSEED-exiting bits on for every guest, regardless of guest
cooperation) — this module only *reads* the capability MSRs today. `ENFORCEMENT_DESIGN.md`
(new, same directory) now has a concrete, source-grounded design for this, replacing the
earlier "likely a `kvm_x86_ops`-level hook or eBPF/kprobe approach" guess: it turns out
kprobes cannot do this at all (the relevant dispatch table is `static` inside
`arch/x86/kvm/vmx/vmx.c`), it has to be a patch to that file rebuilding `kvm_intel.ko`
itself, and — a real finding — KVM's own exit dispatch is bounds/null-checked and falls back
to a clean userspace-visible `KVM_EXIT_INTERNAL_ERROR` rather than panicking on an unwired
exit reason, which meaningfully lowers the risk of attempting this on this same dev host.
Still not built or boot-tested; still on this host in particular the result could never be
fully validated against the RDSEED-exiting bit specifically (RDTSC+RDRAND only).
`crates/baud-host/src/linux.rs`'s `enforced_module_present()` deliberately still returns
`false` unconditionally — wiring it to this probe module would overclaim a regime this host
doesn't actually enforce yet (`regime_is_recorded_and_not_overclaimed`).
