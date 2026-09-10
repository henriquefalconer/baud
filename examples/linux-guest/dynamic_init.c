#include <sys/reboot.h>
#include <sys/io.h>
#include <unistd.h>

/* Same raw-outb bring-up endpoint as init.c (BUILD.md's "Why /init uses raw port I/O" explains
 * why: no interrupt controller here, so a normal write(1, ...) never drains). Unlike every other
 * /init in this directory, this one is compiled *without* -static: it is a real, dynamically-
 * linked glibc binary, whose ld-linux-x86-64.so.2 + libc.so.6 must be resolved out of the
 * initramfs itself at runtime -- proving the pipeline-built initramfs (with its new
 * InitramfsEntry::symlink support, todo.md §14 item 1's H8 prerequisite) can carry a real
 * dynamically-linked rootfs, not just single static binaries. */
static const char marker[] = "baud-guest: dynamically-linked init reached /init\n";

int main(void) {
    iopl(3);
    for (unsigned i = 0; i < sizeof(marker) - 1; i++) {
        outb(marker[i], 0x3f8);
    }
    sync();
    reboot(RB_POWER_OFF);
    for (;;) {
    }
}
