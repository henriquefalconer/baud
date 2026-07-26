#include <sys/reboot.h>
#include <sys/io.h>
#include <unistd.h>

/* Same raw-outb bring-up endpoint as init.c (BUILD.md's "Why /init uses raw port I/O" explains
 * why: no interrupt controller here, so a normal write(1, ...) never drains). */
static const char marker[] = "baud-guest: minimal kernel reached /init\n";

int main(void) {
    iopl(3);
    for (unsigned i = 0; i < sizeof(marker) - 1; i++) {
        outb(marker[i], 0x3f8);
    }
    /* Guest-driven checkpoint (todo.md's own spec for `double_boot_ram_hash_identical`): finalize
     * the tape device's MARK_BRANCH control record (opcode 1, specs/baud-tape-device.md §4) with
     * no payload, so the VMM can hash guest RAM at this exact, guest-chosen instant instead of at
     * a wall-clock point or over raw console text (both of which embed real-hardware RCB/TSC read
     * jitter -- see BUILD.md's "known, deliberate non-goal" section). */
    outb(1, 0x508);
    sync();
    reboot(RB_POWER_OFF);
    for (;;) {
    }
}
