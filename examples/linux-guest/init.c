#include <fcntl.h>
#include <sys/io.h>
#include <sys/ioctl.h>
#include <sys/reboot.h>
#include <unistd.h>

#define BAUD_TAPE_PROBE _IO('B', 0)

static const char marker[] = "baud-guest: minimal kernel reached /init\n";

static void console_write(const char *text, unsigned len) {
    for (unsigned i = 0; i < len; i++)
        outb(text[i], 0x3f8);
}

int main(void) {
    iopl(3);
    console_write(marker, sizeof(marker) - 1);

    int tape = open("/dev/tape", O_RDWR);
    unsigned char input = 0;
    if (tape >= 0) {
        /* One blocking byte is one harness step. The host device supplies the fixed EOT
         * sentinel when the tape is empty, so this path never consults host entropy. */
        if (read(tape, &input, 1) != 1) {
            static const char error[] = "baud-guest: tape read failed\n";
            console_write(error, sizeof(error) - 1);
            close(tape);
            reboot(RB_POWER_OFF);
            for (;;) {}
        }
        /* PROBE payload is a one-byte key length followed by opaque value bytes. An empty
         * key is valid and keeps this harness independent of workload naming. */
        unsigned char record[2] = {0, input};
        if (write(tape, record, sizeof(record)) != (ssize_t)sizeof(record) ||
            ioctl(tape, BAUD_TAPE_PROBE) != 0) {
            static const char error[] = "baud-guest: tape probe failed\n";
            console_write(error, sizeof(error) - 1);
        }
        close(tape);
    } else {
        /* The character-device path needs devtmpfs. Tiny initramfs images often do not mount it,
         * so the documented PIO fallback keeps the endpoint usable before userspace setup. */
        static const char fallback[] = "baud-guest: using PIO tape fallback\n";
        console_write(fallback, sizeof(fallback) - 1);
        input = inb(0x0500);
        outb(0, 0x0500); /* empty PROBE key */
        outb(input, 0x0500);
        outb(0, 0x0508); /* PROBE */
    }
    sync();
    reboot(RB_POWER_OFF);
    for (;;) {}
}
