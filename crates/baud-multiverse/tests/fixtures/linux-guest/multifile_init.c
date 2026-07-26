#include <sys/reboot.h>
#include <sys/io.h>
#include <sys/wait.h>
#include <unistd.h>

/* Same raw-outb bring-up endpoint as init.c (BUILD.md's "Why /init uses raw port I/O" explains
 * why: no interrupt controller here, so a normal write(1, ...) never drains). */
static const char marker[] = "baud-guest: multi-file init reached /init\n";

int main(void) {
    iopl(3);
    for (unsigned i = 0; i < sizeof(marker) - 1; i++) {
        outb(marker[i], 0x3f8);
    }

    /* Exec a second file bundled in the same initramfs (todo.md §14 item 1's open "no real
     * harness-script/agent-binary multi-file rootfs has been assembled or tested yet" gap) --
     * proves a pipeline-built multi-file archive is not just byte-correct but actually usable by
     * PID 1 the way a real workload (e.g. §11's harness + emulator pair) would use it. */
    pid_t pid = fork();
    if (pid == 0) {
        execl("/helper", "/helper", (char *)NULL);
        _exit(127);
    }
    if (pid > 0) {
        int status;
        waitpid(pid, &status, 0);
    }

    sync();
    reboot(RB_POWER_OFF);
    for (;;) {
    }
}
