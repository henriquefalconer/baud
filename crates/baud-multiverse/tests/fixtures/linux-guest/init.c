#include <sys/reboot.h>
#include <sys/io.h>
#include <unistd.h>

/* Bring-up tape/console endpoint (todo.md 4.4 option A): raw port I/O straight to COM1's data
 * register, bypassing the kernel's interrupt-driven tty transmit path entirely -- this machine has
 * no real interrupt controller (`Using NULL legacy PIC`), so a normal write(1, ...) queues into the
 * 8250 driver's ring buffer and waits on an IRQ4 that never fires. */
static const char marker[] = "baud-guest: minimal kernel reached /init\n";

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
