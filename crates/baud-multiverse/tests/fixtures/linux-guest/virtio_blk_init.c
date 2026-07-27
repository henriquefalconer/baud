#include <sys/reboot.h>
#include <sys/io.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <fcntl.h>
#include <unistd.h>

/* Same raw-outb marker convention as virtio_rng_init.c (todo.md 4.4 option A) -- this machine has
 * no interrupt controller a normal write(1, ...) could rely on for the 8250's tty transmit path. */
static void out_bytes(const char *s, unsigned n) {
    for (unsigned i = 0; i < n; i++) {
        outb((unsigned char)s[i], 0x3f8);
    }
}
static void out_str(const char *s) { out_bytes(s, __builtin_strlen(s)); }

static void out_hex(const unsigned char *buf, unsigned n) {
    static const char hex[] = "0123456789abcdef";
    for (unsigned i = 0; i < n; i++) {
        char pair[2] = { hex[(buf[i] >> 4) & 0xf], hex[buf[i] & 0xf] };
        out_bytes(pair, 2);
    }
}

#define SECTOR_SIZE 512

int main(void) {
    iopl(3);
    out_str("baud-guest: minimal kernel reached /init\n");

    /* Same devtmpfsd-race workaround as virtio_rng_init.c: read the real major:minor straight from
     * sysfs (synchronously present the instant virtblk_probe's add_disk() returns) and mknod() the
     * node ourselves rather than waiting on devtmpfs's async node-creation worker. */
    mkdir("/sys", 0755);
    if (mount("sysfs", "/sys", "sysfs", 0, NULL) != 0) {
        out_str("baud-guest: mount-sysfs-failed\n");
        goto done;
    }
    {
        int fd = open("/sys/class/block/vda/dev", O_RDONLY);
        if (fd < 0) {
            out_str("baud-guest: blk-sysfs-dev-missing\n");
            goto done;
        }
        char buf[32] = { 0 };
        ssize_t n = read(fd, buf, sizeof(buf) - 1);
        close(fd);
        if (n <= 0) {
            out_str("baud-guest: blk-sysfs-dev-read-failed\n");
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

        if (mknod("/dev/vda", S_IFBLK | 0600, makedev(major, minor)) != 0) {
            out_str("baud-guest: blk-mknod-failed\n");
            goto done;
        }
    }

    {
        int fd = open("/dev/vda", O_RDWR);
        if (fd < 0) {
            out_str("baud-guest: blk-open-failed\n");
            goto done;
        }
        out_str("baud-guest: blk-open-ok\n");

        /* Sector 0: read the base image's pristine content -- proves a real VIRTIO_BLK_T_IN
         * request round-trips through the legacy virtio-pci transport into the actual backing
         * store the host constructed. */
        unsigned char rbuf[SECTOR_SIZE];
        ssize_t n = read(fd, rbuf, sizeof(rbuf));
        if (n != SECTOR_SIZE) {
            out_str("baud-guest: blk-read-sector0-failed\n");
            goto done_close;
        }
        out_str("baud-guest: blk-sector0-bytes:");
        out_hex(rbuf, SECTOR_SIZE);
        out_str("\n");

        /* Sector 1: write a fixed pattern (VIRTIO_BLK_T_OUT), then read it back (a fresh
         * VIRTIO_BLK_T_IN) to prove the write actually landed in the device's overlay, not just
         * that the write request completed. */
        unsigned char wbuf[SECTOR_SIZE];
        for (unsigned i = 0; i < SECTOR_SIZE; i++) {
            wbuf[i] = (unsigned char)(i & 0xff);
        }
        if (lseek(fd, SECTOR_SIZE, SEEK_SET) != SECTOR_SIZE) {
            out_str("baud-guest: blk-lseek-write-failed\n");
            goto done_close;
        }
        ssize_t wn = write(fd, wbuf, sizeof(wbuf));
        if (wn != SECTOR_SIZE) {
            out_str("baud-guest: blk-write-sector1-failed\n");
            goto done_close;
        }
        out_str("baud-guest: blk-write-sector1-ok\n");

        if (lseek(fd, SECTOR_SIZE, SEEK_SET) != SECTOR_SIZE) {
            out_str("baud-guest: blk-lseek-readback-failed\n");
            goto done_close;
        }
        unsigned char rbuf2[SECTOR_SIZE];
        ssize_t n2 = read(fd, rbuf2, sizeof(rbuf2));
        if (n2 != SECTOR_SIZE) {
            out_str("baud-guest: blk-readback-sector1-failed\n");
            goto done_close;
        }
        out_str("baud-guest: blk-sector1-readback-bytes:");
        out_hex(rbuf2, SECTOR_SIZE);
        out_str("\n");

done_close:
        close(fd);
    }

done:
    sync();
    reboot(RB_POWER_OFF);
    for (;;) {
    }
}
