/*
 * framedemo — M5 display-adapter validation guest
 *
 * Writes a moving gradient as indexed8 frames to stdout (transport: fifo).
 * Each frame is WIDTH * HEIGHT bytes (one byte per pixel = palette index).
 * The gradient pattern: pixel(x,y,t) = (x + t) % 256
 * This scrolls the gradient rightward by one pixel per step.
 *
 * Probes emitted via stdout (stdout-kv format, prefix "baud:"):
 *   baud:step=<N>
 *
 * Guest contract: single-threaded, statically linked, musl.
 * Build: musl-gcc -static -o framedemo framedemo.c
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define DEFAULT_WIDTH  32
#define DEFAULT_HEIGHT 32
#define DEFAULT_STEPS  50

static int parse_int_arg(const char *arg, const char *prefix, int def) {
    size_t plen = strlen(prefix);
    if (strncmp(arg, prefix, plen) == 0) {
        return atoi(arg + plen);
    }
    return def;
}

int main(int argc, char *argv[]) {
    int width  = DEFAULT_WIDTH;
    int height = DEFAULT_HEIGHT;
    int steps  = DEFAULT_STEPS;

    for (int i = 1; i < argc; i++) {
        width  = parse_int_arg(argv[i], "--width=",  width);
        height = parse_int_arg(argv[i], "--height=", height);
        steps  = parse_int_arg(argv[i], "--steps=",  steps);
    }

    int frame_size = width * height;
    unsigned char *frame = malloc(frame_size);
    if (!frame) {
        fprintf(stderr, "framedemo: malloc failed\n");
        return 1;
    }

    for (int t = 0; t < steps; t++) {
        /* Generate indexed8 moving gradient */
        for (int y = 0; y < height; y++) {
            for (int x = 0; x < width; x++) {
                frame[y * width + x] = (unsigned char)((x + t) % 256);
            }
        }

        /* Write frame to stdout (the frame fifo transport) */
        fwrite(frame, 1, frame_size, stdout);
        fflush(stdout);

        /* Emit step probe via stderr (stdout-kv; the supervisor routes this) */
        fprintf(stderr, "baud:step=%d\n", t);
    }

    free(frame);
    return 0;
}
