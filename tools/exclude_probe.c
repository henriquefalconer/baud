// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// exclude_probe — does perf's guest/host execution-mode filter work on this host?
//
// baud's work-clock (retired conditional branches, raw event BR_INST_RETIRED.COND = 0x11c4) must
// count ONLY guest branches. The textbook way is perf_event_attr.exclude_host=1 (count only VMX
// non-root / guest mode). On a nested-virtualized host (e.g. WSL2 under Hyper-V) that filter can
// be inoperative — perf's guest/host discrimination needs the KVM module to register
// `perf_guest_cbs`, which this project's dev host does not do under nested virt — so the counter
// also accrues the host's own userspace branches. That is exactly why baud brackets the counter
// with pause/resume around each KVM_RUN (crates/baud-vcpu/src/linux/mod.rs
// `run_and_convert_rcb_bracketed`; see crates/baud-multiverse/src/linux/mod.rs `LinuxBranchCounter`).
// This probe checks the premise directly, and is the linchpin of the "is pause/resume redundant
// now that the raw 0x11c4 event is used?" A/B experiment.
//
// It opens three counters of the SAME raw event 0x11c4 over one fixed host-userspace loop — plain,
// exclude_host=1 (guest only), exclude_guest=1 (host only). There is NO guest running here, so
// every branch retired is a host branch.
//
// Build & run:
//   sudo sh -c 'echo -1 > /proc/sys/kernel/perf_event_paranoid'
//   cc -O0 -o exclude_probe tools/exclude_probe.c && ./exclude_probe
//
// Interpretation:
//   * exclude_host WORKS  -> plain ~= exclude_guest (both ~20M), and exclude_host ~= 0 (no guest
//     running, so a guest-only counter has nothing to count). If so, the "proper" fix is
//     exclude_host(true) and the pause/resume bracket is the wrong tool — re-open the design.
//   * exclude_host is BROKEN (this host's documented case) -> exclude_host ALSO reads ~20M (it
//     failed to exclude host branches) OR every counter reads 0 / perf_event_open FAILS. Either
//     way perf cannot distinguish guest from host here, so pause/resume is the ONLY mechanism that
//     can make the served RCB guest-only — and whether that still matters given the now-exact raw
//     event is what the drive/h7-enforced-entropy.sh A/B measures.
//   * perf_event_open FAILED (EOPNOTSUPP/ENOENT/ENODEV) -> the raw event isn't exposed at all;
//     confirm the 0x11c4 encoding for this CPU (Intel SDM / libpfm4) and the PMU is present
//     (tools/pmucheck.c). EACCES/EPERM -> raise perf_event_paranoid (the sudo line above).
//
// The raw event 0x11c4 (BR_INST_RETIRED.COND) is the Skylake..Ice-Lake encoding; verify it against
// the Intel SDM / libpfm4 for the exact CPU. See specs/baud-fingerprint.md §9 and
// specs/baud-ubuntu.md §3 for how this feeds the determinism story.

#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <linux/perf_event.h>

// Raw Intel BR_INST_RETIRED.COND: event 0xC4, umask 0x11 -> config 0x11c4 (Skylake..Ice-Lake).
#define BR_INST_RETIRED_COND 0x11c4

static long perf_open(struct perf_event_attr *a) {
    return syscall(SYS_perf_event_open, a, /*pid=*/0, /*cpu=*/-1, /*group=*/-1, /*flags=*/0);
}

// A fixed amount of work with a data-independent, deterministic number of conditional branches.
// `volatile` + -O0 keep the loop from being optimized away.
static unsigned long work(void) {
    volatile unsigned long s = 0;
    for (int i = 0; i < 10000000; i++) {
        if (i & 1) s += i;
        else       s -= i;
    }
    return s;
}

static long long measure(int exclude_host, int exclude_guest, const char *name) {
    struct perf_event_attr a;
    memset(&a, 0, sizeof a);
    a.type = PERF_TYPE_RAW;
    a.size = sizeof a;
    a.config = BR_INST_RETIRED_COND;
    a.disabled = 1;
    a.pinned = 1;                     // dedicate a hardware counter (no multiplexing estimate)
    a.exclude_hv = 1;
    a.exclude_host = exclude_host;    // 1 => count only guest (VMX non-root)
    a.exclude_guest = exclude_guest;  // 1 => count only host

    long fd = perf_open(&a);
    if (fd < 0) {
        printf("  %-28s perf_event_open FAILED: %s\n", name, strerror(errno));
        return -1;
    }
    ioctl(fd, PERF_EVENT_IOC_RESET, 0);
    ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
    work();
    ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);

    long long v = 0;
    if (read(fd, &v, sizeof v) < 0) v = -1;
    close(fd);
    return v;
}

int main(void) {
    printf("exclude_probe: pure HOST userspace work, raw event 0x11c4 (BR_INST_RETIRED.COND)\n\n");
    printf("  plain (no exclude)          = %lld\n", measure(0, 0, "plain"));
    printf("  exclude_host=1 (guest only) = %lld\n", measure(1, 0, "exclude_host"));
    printf("  exclude_guest=1 (host only) = %lld\n", measure(0, 1, "exclude_guest"));
    printf("\nIf exclude_host WORKS:  plain ~= exclude_guest (~20M), exclude_host ~= 0.\n");
    printf("If exclude_host is BROKEN here: exclude_host also reads ~20M (it counted host\n");
    printf("  branches it was told to exclude), or every counter reads 0 / open FAILS.\n");
    return 0;
}
