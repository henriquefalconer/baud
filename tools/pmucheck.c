// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// pmucheck — host PMU probe: is the retired-branch counter present and deterministic here?
//
// baud's work-clock is retired conditional branches, read via perf_event_open on the vCPU
// thread. Under nested virtualization (e.g. WSL2 under Hyper-V) the hardware PMU may be masked,
// which would make the work-clock — and therefore the cross-VM determinism fingerprint —
// unsourceable on that host. Run this on the dev box to check availability + userspace
// determinism before trusting it. See specs/baud-fingerprint.md §9 and specs/baud-ubuntu.md §3.
//
// Build & run:
//   sudo sh -c 'echo -1 > /proc/sys/kernel/perf_event_paranoid'
//   cc -O0 -o pmucheck tools/pmucheck.c && ./pmucheck
//
// Interpretation:
//   * perf_event_open FAILED (EOPNOTSUPP / ENOENT / ENODEV) -> the hardware PMU is not exposed
//     on this host (typical for a nested Hyper-V / WSL2 guest) -> source the work-clock and run
//     the fingerprint on bare metal instead.
//   * EACCES / EPERM -> raise the paranoia setting (the sudo line above) or run as root.
//   * identical HW_BRANCH_INSTRUCTIONS across all runs -> the counter is present and
//     deterministic in userspace. Any variance / 0 / wild swings -> emulated or multiplexed,
//     not trustworthy.
//   The authoritative *guest-level* gate is `baud host probe` / drive/h0.sh's
//   rcb_is_deterministic_on_this_cpu (H0), which boots a fixed guest loop under KVM twice and
//   compares the guest-filtered branch count. This tool is a fast pre-check for that.
//
// The raw event 0x11c4 (BR_INST_RETIRED.COND) is the Skylake..Ice-Lake encoding; the raw
// encoding is microarchitecture-specific, so verify it against the Intel SDM / libpfm4 for the
// exact CPU. The generic PERF_COUNT_HW_BRANCH_INSTRUCTIONS test is the reliable availability probe.

#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <linux/perf_event.h>

static long perf_open(struct perf_event_attr *a) {
    return syscall(SYS_perf_event_open, a, 0 /* this thread */, -1 /* any cpu */, -1, 0);
}

// A fixed amount of work whose retired-branch count is data-independent and deterministic.
static unsigned long work(void) {
    volatile unsigned long s = 0;
    for (int i = 0; i < 10000000; i++)
        if (i & 1) s += i; else s -= i;
    return s;
}

static long long measure(unsigned type, unsigned long cfg, const char *name) {
    struct perf_event_attr a;
    memset(&a, 0, sizeof a);
    a.type = type;
    a.size = sizeof a;
    a.config = cfg;
    a.disabled = 1;
    a.pinned = 1;              // dedicate a counter; never multiplex (multiplexing => estimate)
    a.exclude_kernel = 1;      // count only user-space of this thread
    a.exclude_hv = 1;

    long fd = perf_open(&a);
    if (fd < 0) {
        printf("  %-24s perf_event_open FAILED: %s\n", name, strerror(errno));
        return -1;
    }
    ioctl(fd, PERF_EVENT_IOC_RESET, 0);
    ioctl(fd, PERF_EVENT_IOC_ENABLE, 0);
    work();
    ioctl(fd, PERF_EVENT_IOC_DISABLE, 0);

    long long v = 0;
    if (read(fd, &v, sizeof v) < 0)
        v = -1;
    close(fd);
    return v;
}

int main(void) {
    printf("pmucheck: retired-branch PMU availability + determinism\n\n");

    printf("[1] generic hardware event (availability probe):\n");
    for (int r = 1; r <= 3; r++)
        printf("    run %d  HW_BRANCH_INSTRUCTIONS = %lld\n", r,
               measure(PERF_TYPE_HARDWARE, PERF_COUNT_HW_BRANCH_INSTRUCTIONS,
                       "HW_BRANCH_INSTRUCTIONS"));

    printf("\n[2] raw BR_INST_RETIRED.COND (0x11c4; uarch-specific, verify per CPU):\n");
    for (int r = 1; r <= 3; r++)
        printf("    run %d  BR_INST_RETIRED.COND  = %lld\n", r,
               measure(PERF_TYPE_RAW, 0x11c4, "BR_INST_RETIRED.COND"));

    printf("\nPASS if [1] prints three identical, non-zero counts. See the header for how to read failures.\n");
    return 0;
}
