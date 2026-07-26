#include <sys/io.h>

/* Same raw-outb bring-up endpoint as init.c (BUILD.md's "Why /init uses raw port I/O" explains
 * why: no interrupt controller here, so a normal write(1, ...) never drains). This binary is a
 * second file bundled alongside /init in the initramfs (see multifile_init.c) -- its own marker
 * proves the rootfs archive genuinely carries more than one file and that /init actually execs
 * this one, not just that a single-file init works. */
static const char marker[] = "baud-guest: helper executed from a multi-file initramfs\n";

int main(void) {
    iopl(3);
    for (unsigned i = 0; i < sizeof(marker) - 1; i++) {
        outb(marker[i], 0x3f8);
    }
    return 0;
}
