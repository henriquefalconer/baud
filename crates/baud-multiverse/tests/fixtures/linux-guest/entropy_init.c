#include <fcntl.h>
#include <sys/io.h>
#include <sys/mount.h>
#include <sys/random.h>
#include <sys/reboot.h>
#include <sys/stat.h>
#include <unistd.h>

/* todo.md §14 item 2 / H7 `os_entropy_is_deterministic`: reads OS entropy four times via the
 * `getrandom()` syscall and four times via `/dev/urandom`, hex-encodes each read, and writes the
 * result out via raw port I/O to COM1 -- the same `iopl(3)`+`outb` bring-up endpoint `init.c` uses
 * and for the same reason (this machine has no interrupt controller, so the 8250 driver's normal
 * interrupt-driven tty transmit path never drains -- see this directory's BUILD.md). */

#define PROBE_LEN 32

static void out_byte(unsigned char b) {
    outb(b, 0x3f8);
}

static void out_str(const char *s) {
    while (*s) {
        out_byte((unsigned char)*s++);
    }
}

static void out_hex(const unsigned char *buf, unsigned n) {
    static const char hex[] = "0123456789abcdef";
    for (unsigned i = 0; i < n; i++) {
        out_byte(hex[buf[i] >> 4]);
        out_byte(hex[buf[i] & 0xf]);
    }
}

int main(void) {
    iopl(3);

    unsigned char buf[PROBE_LEN];

    for (int i = 0; i < 4; i++) {
        out_str("GETRANDOM:");
        ssize_t n = getrandom(buf, PROBE_LEN, 0);
        if (n == PROBE_LEN) {
            out_hex(buf, PROBE_LEN);
        } else {
            out_str("ERR");
        }
        out_byte('\n');
    }

    /* rdinit=/init (not root=/prepare_namespace()) skips the kernel's own devtmpfs auto-mount, so
     * the initramfs boots with no /dev at all -- CONFIG_DEVTMPFS_MOUNT is a no-op on this boot path.
     * Mount it ourselves so /dev/urandom exists. */
    mkdir("/dev", 0755);
    mount("devtmpfs", "/dev", "devtmpfs", 0, NULL);

    int fd = open("/dev/urandom", O_RDONLY);
    for (int i = 0; i < 4; i++) {
        out_str("URANDOM:");
        if (fd >= 0) {
            ssize_t got = 0;
            while (got < PROBE_LEN) {
                ssize_t n = read(fd, buf + got, PROBE_LEN - got);
                if (n <= 0) {
                    break;
                }
                got += n;
            }
            if (got == PROBE_LEN) {
                out_hex(buf, PROBE_LEN);
            } else {
                out_str("ERR");
            }
        } else {
            out_str("NOFD");
        }
        out_byte('\n');
    }

    static const char marker[] = "baud-guest: entropy probe done\n";
    out_str(marker);

    sync();
    reboot(RB_POWER_OFF);
    for (;;) {
    }
}
