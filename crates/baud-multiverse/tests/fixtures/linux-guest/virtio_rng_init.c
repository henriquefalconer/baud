#include <sys/reboot.h>
#include <sys/io.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <fcntl.h>
#include <unistd.h>

/* Same raw-outb marker convention as init.c (todo.md 4.4 option A) -- this machine has no
 * interrupt controller a normal write(1, ...) could rely on for the 8250's tty transmit path. */
static void out_bytes(const char *s, unsigned n) {
    for (unsigned i = 0; i < n; i++) {
        outb((unsigned char)s[i], 0x3f8);
    }
}
static void out_str(const char *s) { out_bytes(s, __builtin_strlen(s)); }

int main(void) {
    iopl(3);
    out_str("baud-guest: minimal kernel reached /init\n");

    /* CONFIG_DEVTMPFS_MOUNT already populates most of /dev before /init runs, but node creation
     * for a device registered late in do_initcalls() is handled by an async kernel worker thread
     * (devtmpfsd) that this single-vCPU deterministic machine gives no guaranteed chance to run
     * before /init's very first instructions execute -- confirmed empirically: /sys/class/misc/
     * hw_random and /proc/interrupts' "virtio0" both exist (the driver really did probe and bind
     * its IRQ), yet stat("/dev/hwrng") reliably fails. Waiting for it (sleep/sched_yield) would be
     * a real, un-bounded race; instead this reads the device's real major:minor straight from
     * sysfs (deterministic, always present the instant misc_register() returns) and mknod()s the
     * node itself. */
    mkdir("/sys", 0755);
    if (mount("sysfs", "/sys", "sysfs", 0, NULL) != 0) {
        out_str("baud-guest: mount-sysfs-failed\n");
        goto done;
    }
    {
        int fd = open("/sys/class/misc/hw_random/dev", O_RDONLY);
        if (fd < 0) {
            out_str("baud-guest: hwrng-sysfs-dev-missing\n");
            goto done;
        }
        char buf[32] = { 0 };
        ssize_t n = read(fd, buf, sizeof(buf) - 1);
        close(fd);
        if (n <= 0) {
            out_str("baud-guest: hwrng-sysfs-dev-read-failed\n");
            goto done;
        }
        unsigned major = 0, minor = 0;
        const char *p = buf;
        while (*p >= '0' && *p <= '9') {
            major = major * 10 + (unsigned)(*p - '0');
            p++;
        }
        if (*p == ':') {
            p++;
        }
        while (*p >= '0' && *p <= '9') {
            minor = minor * 10 + (unsigned)(*p - '0');
            p++;
        }

        if (mknod("/dev/hwrng", S_IFCHR | 0600, makedev(major, minor)) != 0) {
            out_str("baud-guest: hwrng-mknod-failed\n");
            goto done;
        }
    }

    {
        int fd = open("/dev/hwrng", O_RDONLY);
        if (fd < 0) {
            out_str("baud-guest: hwrng-open-failed\n");
            goto done;
        }
        out_str("baud-guest: hwrng-open-ok\n");

        /* Read multiple times, not once: a single read only proves the *initial* virtio-rng
         * completion is deterministic. Looping exercises repeated request/completion round-trips
         * through the same open fd -- the "continuous reseeding" spec §3.8 names -- so the run
         * genuinely re-notifies the device and re-drains the entropy stream several times per
         * boot, not just once. */
        static const char hex[] = "0123456789abcdef";
        for (int round = 0; round < 4; round++) {
            unsigned char rbuf[16];
            ssize_t n = read(fd, rbuf, sizeof(rbuf));
            if (n <= 0) {
                out_str("baud-guest: hwrng-read-failed\n");
                goto done;
            }
            out_str("baud-guest: hwrng-bytes:");
            for (ssize_t i = 0; i < n; i++) {
                char pair[2] = { hex[(rbuf[i] >> 4) & 0xf], hex[rbuf[i] & 0xf] };
                out_bytes(pair, 2);
            }
            out_str("\n");
        }
        close(fd);
    }

done:
    sync();
    reboot(RB_POWER_OFF);
    for (;;) {
    }
}
