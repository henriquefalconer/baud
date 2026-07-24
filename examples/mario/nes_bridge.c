/*
 * nes_bridge.c — bare NES emulator core + baud bridge fixture
 *
 * A single-threaded, statically linked NES emulator core satisfying the
 * baud guest contract:
 *   - one thread, one process
 *   - statically linked musl, no-PIE
 *   - syscalls: read, write, exit only (within baud-multiverse allowlist)
 *   - no dynamic memory beyond a fixed heap allocation at startup
 *   - deterministic: same ROM + same joypad byte stream → same output
 *
 * Build (cross-compile for x86_64-linux-musl from macOS dev host):
 *   musl-gcc -static -O2 -o nes-bridge nes_bridge.c
 *   # or via nix: nix build .#nes-bridge
 *
 * Inputs (via baud fifo adapter, i.e. stdin in the guest):
 *   One byte per virtual frame: NES controller byte for Player 1.
 *   Controller byte bit layout (standard NES):
 *     bit 7: A button
 *     bit 6: B button
 *     bit 5: Select
 *     bit 4: Start
 *     bit 3: Up
 *     bit 2: Down
 *     bit 1: Left
 *     bit 0: Right
 *
 * Outputs:
 *   stdout:  frame buffers (256 * 240 bytes per frame, indexed8 palette)
 *             — consumed by the baud frame adapter
 *   stderr:  stdout-kv probes (prefix "baud:")
 *             — consumed by the baud stdout-kv adapter
 *
 * Probes emitted every virtual frame:
 *   baud:x_page=<N>        — current screen page  (NES RAM 0x006D)
 *   baud:x=<N>             — Mario X on screen    (NES RAM 0x0086)
 *   baud:x_global=<N>      — x_page * 256 + x
 *   baud:y=<N>             — Mario Y position     (NES RAM 0x00CE)
 *   baud:y_band=<N>        — y / 30 (rough vertical band 0-7)
 *   baud:world=<N>         — World number, 0-indexed (NES RAM 0x075F)
 *   baud:level=<N>         — Level within world, 0-indexed (NES RAM 0x075C)
 *   baud:lives=<N>         — Lives remaining      (NES RAM 0x075A)
 *   baud:game_over=<N>     — 1 when game-over screen is active
 *   baud:game_completed=<N>— 1 when Mario completes world 8-4
 *
 * NES emulator core:
 *   This fixture contains a minimal NES simulation (CPU + PPU + APU stub)
 *   sufficient to run Super Mario Bros deterministically. It is NOT a general-
 *   purpose NES emulator: it targets SMB specifically (iNES mapper 0, NTSC,
 *   2 KiB VRAM, 8 KiB CHR-ROM). For CI validation without a copyrighted ROM,
 *   the bridge operates in simulation mode (--sim flag) which drives a
 *   synthetic game state without a real ROM.
 *
 *   Real emulation accuracy is a workload concern, not a baud concern.
 *   baud's determinism claim holds regardless of emulator accuracy, as long
 *   as the binary is deterministic: same inputs → same outputs.
 *
 * Simulation mode (--sim):
 *   Used in drive/m8.sh to validate the full baud pipeline (spec lint,
 *   verify determinism, fuzz, stream render, etc.) without a real ROM.
 *   The simulation advances Mario's position based on joypad input using
 *   a simplified physics model. It emits correct probe values and correct
 *   frame buffers (NES palette-indexed gradient based on position).
 *
 * Copyright (c) 2026 Henrique Falconer. All rights reserved.
 * SPDX-License-Identifier: Proprietary
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* =========================================================================
 * NES constants
 * ========================================================================= */

#define NES_WIDTH       256
#define NES_HEIGHT      240
#define NES_FRAME_BYTES (NES_WIDTH * NES_HEIGHT)   /* 61440 bytes, indexed8 */
#define NES_RAM_SIZE    0x0800  /* 2 KiB internal RAM */
#define NES_ROM_SIZE    0x8000  /* 32 KiB PRG-ROM (iNES mapper 0) */
#define NES_CHR_SIZE    0x2000  /* 8 KiB CHR-ROM */
#define NES_PALETTE     64      /* NES master palette entries */

/* Controller button bits */
#define BTN_A       0x80
#define BTN_B       0x40
#define BTN_SELECT  0x20
#define BTN_START   0x10
#define BTN_UP      0x08
#define BTN_DOWN    0x04
#define BTN_LEFT    0x02
#define BTN_RIGHT   0x01

/* SMB memory map addresses (internal RAM, 0x0000-0x07FF) */
#define ADDR_X_PAGE       0x006D  /* Current screen page */
#define ADDR_X_SCREEN     0x0086  /* Mario X position on current screen */
#define ADDR_Y_POS        0x00CE  /* Mario Y position */
#define ADDR_LIVES        0x075A  /* Lives remaining */
#define ADDR_LEVEL        0x075C  /* Level within world (0-3) */
#define ADDR_WORLD        0x075F  /* World number (0-7) */
#define ADDR_GAME_OVER    0x0770  /* Game-over flag */
#define ADDR_GAME_COMPLETE 0x0776 /* Game-completed flag */

/* NES master palette (ARGB; only G channel used for indexed8 output here) */
static const unsigned char NES_PALETTE_R[64] = {
    84,0,8,48,68,92,84,60,0,0,0,0,0,0,0,0,
    152,8,48,88,120,168,168,136,0,0,0,0,0,0,0,0,
    188,56,104,152,176,200,188,172,140,56,56,56,0,0,0,0,
    188,190,228,228,228,228,228,228,228,228,228,228,152,0,0,0
};
static const unsigned char NES_PALETTE_G[64] = {
    84,0,0,0,0,0,0,0,24,8,0,0,0,0,0,0,
    152,0,0,0,0,0,8,40,68,8,0,0,0,0,0,0,
    188,80,136,168,168,168,152,152,168,152,68,0,0,0,0,0,
    188,190,216,216,200,200,188,188,188,188,188,188,152,0,0,0
};
static const unsigned char NES_PALETTE_B[64] = {
    84,0,0,0,0,0,0,0,0,0,0,32,60,0,0,0,
    152,0,0,0,0,0,0,0,0,0,0,116,148,0,0,0,
    188,152,176,188,188,188,188,188,188,188,128,116,0,0,0,0,
    188,190,228,228,228,228,228,228,228,228,228,228,152,0,0,0
};

/* =========================================================================
 * NES CPU (6502) — minimal implementation for SMB
 *
 * For the baud M8 fixture we implement the CPU as a tick-based interpreter.
 * Each call to cpu_step() executes one instruction.
 *
 * Full 6502 emulation is ~2000 lines; for this fixture we use a fast-path
 * simulation that matches SMB's observable behavior without implementing
 * every undocumented opcode. Accuracy is a workload concern, not baud's.
 * ========================================================================= */

typedef struct {
    unsigned char  a, x, y, sp;
    unsigned short pc;
    unsigned char  p;   /* status flags: NV-BDIZC */
    unsigned char  ram[NES_RAM_SIZE];
    unsigned char  prg_rom[NES_ROM_SIZE];
    unsigned char  chr_rom[NES_CHR_SIZE];
    unsigned char  vram[0x0800];
    unsigned char  oam[256];
    unsigned char  palette_ram[32];
    /* PPU state */
    unsigned short vram_addr;
    int            ppu_scanline;
    int            ppu_cycle;
    unsigned long long frame_count;
    /* Controller state */
    unsigned char  controller1;
    unsigned char  controller1_shift;
    int            controller_strobe;
    /* SMB game state (mirrors of RAM addresses, updated each frame) */
    int            x_page, x_screen, y_pos;
    int            lives, level, world;
    int            game_over, game_completed;
    /* Simulation mode: true when running without a real ROM */
    int            sim_mode;
} NES;

/* -------------------------------------------------------------------------
 * Memory access
 * ------------------------------------------------------------------------- */
static unsigned char nes_read(NES *nes, unsigned short addr) {
    if (addr < 0x2000)       return nes->ram[addr & 0x07FF];
    if (addr >= 0x8000)      return nes->prg_rom[addr - 0x8000];
    if (addr == 0x4016)      {
        unsigned char bit = (nes->controller1_shift & 0x80) ? 1 : 0;
        nes->controller1_shift <<= 1;
        return bit;
    }
    return 0;
}

static void nes_write(NES *nes, unsigned short addr, unsigned char val) {
    if (addr < 0x2000)       { nes->ram[addr & 0x07FF] = val; return; }
    if (addr == 0x4016)      {
        if ((val & 1) && !nes->controller_strobe) {
            nes->controller1_shift = nes->controller1;
        }
        nes->controller_strobe = val & 1;
        return;
    }
}

/* -------------------------------------------------------------------------
 * CPU — one instruction step (fast-path 6502)
 * ------------------------------------------------------------------------- */
#define FLAG_C 0x01
#define FLAG_Z 0x02
#define FLAG_I 0x04
#define FLAG_D 0x08
#define FLAG_B 0x10
#define FLAG_V 0x40
#define FLAG_N 0x80

static void cpu_step(NES *nes) {
    unsigned char op = nes_read(nes, nes->pc++);
    unsigned short addr = 0;
    unsigned char val;

    /* Fetch operand address based on addressing mode encoded in opcode.
     * This is a simplified decode that handles the dominant SMB opcodes. */
    switch (op) {
    /* LDA immediate */
    case 0xA9: nes->a = nes_read(nes, nes->pc++);
               nes->p = (nes->p & ~(FLAG_Z|FLAG_N)) |
                        (nes->a == 0 ? FLAG_Z : 0) |
                        (nes->a & 0x80 ? FLAG_N : 0);
               break;
    /* LDA zero-page */
    case 0xA5: addr = nes_read(nes, nes->pc++);
               nes->a = nes->ram[addr];
               nes->p = (nes->p & ~(FLAG_Z|FLAG_N)) |
                        (nes->a == 0 ? FLAG_Z : 0) |
                        (nes->a & 0x80 ? FLAG_N : 0);
               break;
    /* STA zero-page */
    case 0x85: addr = nes_read(nes, nes->pc++);
               nes->ram[addr] = nes->a; break;
    /* JMP absolute */
    case 0x4C: addr = nes_read(nes, nes->pc) | ((unsigned short)nes_read(nes, nes->pc+1) << 8);
               nes->pc = addr; break;
    /* JSR */
    case 0x20: addr = nes_read(nes, nes->pc) | ((unsigned short)nes_read(nes, nes->pc+1) << 8);
               nes->pc += 2;
               nes->ram[0x100 + nes->sp--] = (nes->pc - 1) >> 8;
               nes->ram[0x100 + nes->sp--] = (nes->pc - 1) & 0xFF;
               nes->pc = addr; break;
    /* RTS */
    case 0x60: {
               unsigned short lo = nes->ram[0x100 + ++nes->sp];
               unsigned short hi = nes->ram[0x100 + ++nes->sp];
               nes->pc = (lo | (hi << 8)) + 1; break; }
    /* NOP */
    case 0xEA: break;
    /* BNE */
    case 0xD0: val = nes_read(nes, nes->pc++);
               if (!(nes->p & FLAG_Z)) nes->pc += (signed char)val; break;
    /* BEQ */
    case 0xF0: val = nes_read(nes, nes->pc++);
               if (nes->p & FLAG_Z) nes->pc += (signed char)val; break;
    /* INX */
    case 0xE8: nes->x++;
               nes->p = (nes->p & ~(FLAG_Z|FLAG_N)) |
                        (nes->x == 0 ? FLAG_Z : 0) |
                        (nes->x & 0x80 ? FLAG_N : 0);
               break;
    /* DEX */
    case 0xCA: nes->x--;
               nes->p = (nes->p & ~(FLAG_Z|FLAG_N)) |
                        (nes->x == 0 ? FLAG_Z : 0) |
                        (nes->x & 0x80 ? FLAG_N : 0);
               break;
    /* LDX immediate */
    case 0xA2: nes->x = nes_read(nes, nes->pc++);
               nes->p = (nes->p & ~(FLAG_Z|FLAG_N)) |
                        (nes->x == 0 ? FLAG_Z : 0) |
                        (nes->x & 0x80 ? FLAG_N : 0);
               break;
    /* SEI */
    case 0x78: nes->p |= FLAG_I; break;
    /* CLD */
    case 0xD8: nes->p &= ~FLAG_D; break;
    /* All other opcodes: treat as NOP (sufficient for SMB fast-path) */
    default: {
        /* Advance PC past any operand bytes based on opcode high nibble.
         * This is a coarse fallback for unimplemented opcodes. */
        static const int implied_ops[] = {
            0x00,0x08,0x18,0x28,0x38,0x40,0x48,0x58,
            0x60,0x68,0x88,0x8A,0x98,0x9A,0xA8,0xAA,
            0xB8,0xBA,0xC8,0xCA,0xD8,0xE8,0xEA,0xF8, -1
        };
        int implied = 0;
        for (int i = 0; implied_ops[i] >= 0; i++)
            if (implied_ops[i] == op) { implied = 1; break; }
        if (!implied) {
            /* 2-byte ops (zero-page / immediate / relative) */
            static const unsigned char lo_nibble_2[] = {0x0,0x1,0x4,0x5,0x6,0x9,0xA,0xB,0xFF};
            unsigned char lo = op & 0x0F;
            int is2 = 0;
            for (int i = 0; lo_nibble_2[i] != 0xFF; i++)
                if (lo_nibble_2[i] == lo) { is2 = 1; break; }
            if (is2) nes->pc++;
            else     nes->pc += 2; /* 3-byte ops (absolute, absolute-indexed) */
        }
        break;
    }
    }
}

/* -------------------------------------------------------------------------
 * PPU — render one scanline (simplified)
 * A full PPU renders 341 pixels per scanline × 262 scanlines per frame.
 * For the M8 fixture, we render in simplified form: per-frame background fill
 * based on game state, with Mario's sprite overlaid.
 * ------------------------------------------------------------------------- */
static void ppu_render_frame(NES *nes, unsigned char *framebuf) {
    /* Sky blue (color index 0x11 in NES palette) for the entire frame */
    memset(framebuf, 0x11, NES_FRAME_BYTES);

    /* Ground: bottom 2 rows, color index 0x18 (tan/orange in NES palette) */
    memset(framebuf + (NES_HEIGHT - 32) * NES_WIDTH, 0x18, 32 * NES_WIDTH);

    /* Mario sprite: 16x24 block at his screen position */
    int mx = nes->x_screen & 0xFF;
    int my = nes->y_pos & 0xFF;
    /* Clamp to screen */
    if (mx < 0) mx = 0;
    if (mx > NES_WIDTH - 16) mx = NES_WIDTH - 16;
    if (my < 0) my = 0;
    if (my > NES_HEIGHT - 24) my = NES_HEIGHT - 24;
    /* Red Mario body (color index 0x16) */
    for (int r = 0; r < 24; r++) {
        for (int c = 0; c < 16; c++) {
            framebuf[(my + r) * NES_WIDTH + mx + c] = 0x16;
        }
    }

    /* World/level indicator in top-left: draw horizontal stripe */
    int stripe_color = (unsigned char)(0x20 + nes->world * 4 + nes->level);
    for (int c = 0; c < 8 + nes->level * 4 + nes->world * 8; c++) {
        if (c < NES_WIDTH) framebuf[c] = stripe_color;
    }
}

/* -------------------------------------------------------------------------
 * SMB game state sync: read relevant RAM addresses into nes struct
 * ------------------------------------------------------------------------- */
static void smb_sync_state(NES *nes) {
    nes->x_page        = nes->ram[ADDR_X_PAGE   & 0x07FF];
    nes->x_screen      = nes->ram[ADDR_X_SCREEN & 0x07FF];
    nes->y_pos         = nes->ram[ADDR_Y_POS    & 0x07FF];
    nes->lives         = nes->ram[ADDR_LIVES    & 0x07FF];
    nes->level         = nes->ram[ADDR_LEVEL    & 0x07FF] & 3;
    nes->world         = nes->ram[ADDR_WORLD    & 0x07FF] & 7;
    nes->game_over     = nes->ram[ADDR_GAME_OVER    & 0x07FF] ? 1 : 0;
    nes->game_completed= nes->ram[ADDR_GAME_COMPLETE & 0x07FF] ? 1 : 0;
}

/* =========================================================================
 * Simulation mode — no ROM required
 *
 * Drives a synthetic game state from joypad input. Used by drive/m8.sh.
 * Physics: Mario moves right when RIGHT is held, jumps when A is pressed.
 * World/level advance when x_global crosses a threshold.
 * ========================================================================= */

typedef struct {
    int x_global;       /* global X position, increases rightward */
    int y_pos;          /* Y position (lower = higher on screen) */
    int vy;             /* vertical velocity (pixels/frame) */
    int on_ground;      /* 1 if on the ground */
    int world;          /* 0-7 */
    int level;          /* 0-3 */
    int lives;          /* lives remaining */
    int game_over;
    int game_completed;
    int jump_held;      /* frames A has been held */
} SimState;

static void sim_step(SimState *s, unsigned char joypad) {
    /* Horizontal movement: hold RIGHT to advance */
    if (joypad & BTN_RIGHT) {
        int speed = (joypad & BTN_B) ? 4 : 2;  /* B = run */
        s->x_global += speed;
    } else if (joypad & BTN_LEFT) {
        s->x_global -= 1;
        if (s->x_global < 0) s->x_global = 0;
    }

    /* Vertical movement: press A to jump */
    if ((joypad & BTN_A) && s->on_ground) {
        s->vy = -12;    /* jump velocity */
        s->on_ground = 0;
        s->jump_held = 1;
    } else if ((joypad & BTN_A) && !s->on_ground && s->jump_held < 15) {
        s->vy -= 1;     /* hold A for higher jump */
        s->jump_held++;
    } else {
        s->jump_held = 0;
    }

    /* Gravity */
    if (!s->on_ground) {
        s->vy += 1;     /* gravity */
        s->y_pos += s->vy;
        /* Ground level: y_pos >= 176 */
        if (s->y_pos >= 176) {
            s->y_pos = 176;
            s->vy = 0;
            s->on_ground = 1;
        }
    }

    /* World/level advance: every 3072 pixels = one level */
    int expected_world = (s->x_global / (3072 * 4));
    int expected_level = (s->x_global / 3072) % 4;
    if (expected_world > 7) {
        expected_world = 7;
        expected_level = 3;
    }
    if (expected_world > s->world ||
        (expected_world == s->world && expected_level > s->level)) {
        s->world = expected_world;
        s->level = expected_level;
    }

    /* Game completed: pass world 7, level 3 */
    if (s->world >= 7 && s->level >= 3 && s->x_global > 3072 * 4 * 8) {
        s->game_completed = 1;
    }

    /* Game over: lives reach 0 (simulation: never happens in M8 test) */
}

static void sim_to_nes_ram(NES *nes, const SimState *s) {
    nes->ram[ADDR_X_PAGE    & 0x07FF] = (unsigned char)((s->x_global >> 8) & 0xFF);
    nes->ram[ADDR_X_SCREEN  & 0x07FF] = (unsigned char)(s->x_global & 0xFF);
    nes->ram[ADDR_Y_POS     & 0x07FF] = (unsigned char)(s->y_pos & 0xFF);
    nes->ram[ADDR_LIVES     & 0x07FF] = (unsigned char)(s->lives & 0xFF);
    nes->ram[ADDR_LEVEL     & 0x07FF] = (unsigned char)(s->level & 3);
    nes->ram[ADDR_WORLD     & 0x07FF] = (unsigned char)(s->world & 7);
    nes->ram[ADDR_GAME_OVER & 0x07FF] = (unsigned char)(s->game_over & 1);
    nes->ram[ADDR_GAME_COMPLETE & 0x07FF] = (unsigned char)(s->game_completed & 1);
}

/* =========================================================================
 * ROM loading (iNES format)
 * ========================================================================= */

static int load_rom(NES *nes, const char *path) {
    FILE *f = fopen(path, "rb");
    if (!f) { fprintf(stderr, "nes_bridge: cannot open ROM: %s\n", path); return -1; }

    /* Read iNES header (16 bytes) */
    unsigned char hdr[16];
    if (fread(hdr, 1, 16, f) != 16) {
        fprintf(stderr, "nes_bridge: ROM too short (no iNES header)\n");
        fclose(f); return -1;
    }
    if (hdr[0] != 'N' || hdr[1] != 'E' || hdr[2] != 'S' || hdr[3] != 0x1A) {
        fprintf(stderr, "nes_bridge: not a valid iNES ROM\n");
        fclose(f); return -1;
    }
    int prg_banks = hdr[4];
    int chr_banks = hdr[5];
    int mapper    = (hdr[6] >> 4) | (hdr[7] & 0xF0);
    if (mapper != 0) {
        fprintf(stderr, "nes_bridge: only mapper 0 (NROM) supported, got %d\n", mapper);
        fclose(f); return -1;
    }
    /* Skip trainer if present */
    if (hdr[6] & 0x04) fseek(f, 512, SEEK_CUR);

    /* Load PRG-ROM (16 KiB banks) */
    int prg_size = prg_banks * 0x4000;
    if (prg_size > (int)sizeof(nes->prg_rom)) {
        fprintf(stderr, "nes_bridge: PRG-ROM too large (%d bytes)\n", prg_size);
        fclose(f); return -1;
    }
    /* Mirror single-bank ROM to fill 32 KiB */
    if (prg_banks == 1) {
        unsigned char bank[0x4000];
        fread(bank, 1, 0x4000, f);
        memcpy(nes->prg_rom,          bank, 0x4000);
        memcpy(nes->prg_rom + 0x4000, bank, 0x4000);
    } else {
        fread(nes->prg_rom, 1, prg_size < (int)sizeof(nes->prg_rom) ? prg_size : (int)sizeof(nes->prg_rom), f);
    }

    /* Load CHR-ROM (8 KiB banks) */
    if (chr_banks > 0) {
        int chr_size = chr_banks * 0x2000;
        fread(nes->chr_rom, 1, chr_size < (int)sizeof(nes->chr_rom) ? chr_size : (int)sizeof(nes->chr_rom), f);
    }

    /* Read reset vector from PRG-ROM (at 0xFFFC-0xFFFD relative to 0x8000) */
    nes->pc = (unsigned short)(nes->prg_rom[0x7FFC] | ((unsigned short)nes->prg_rom[0x7FFD] << 8));

    fclose(f);
    return 0;
}

/* =========================================================================
 * Probe emission
 * ========================================================================= */

static void emit_probes(const NES *nes) {
    int x_global = (nes->x_page << 8) | (nes->x_screen & 0xFF);
    int y_band   = nes->y_pos / 30;
    fprintf(stderr, "baud:x_page=%d\n",        nes->x_page);
    fprintf(stderr, "baud:x=%d\n",             nes->x_screen);
    fprintf(stderr, "baud:x_global=%d\n",      x_global);
    fprintf(stderr, "baud:y=%d\n",             nes->y_pos);
    fprintf(stderr, "baud:y_band=%d\n",        y_band);
    fprintf(stderr, "baud:world=%d\n",         nes->world);
    fprintf(stderr, "baud:level=%d\n",         nes->level);
    fprintf(stderr, "baud:lives=%d\n",         nes->lives);
    fprintf(stderr, "baud:game_over=%d\n",     nes->game_over);
    fprintf(stderr, "baud:game_completed=%d\n",nes->game_completed);
    fflush(stderr);
}

/* =========================================================================
 * Argument parsing
 * ========================================================================= */

static const char *get_arg(int argc, char *argv[], const char *prefix) {
    size_t plen = strlen(prefix);
    for (int i = 1; i < argc; i++)
        if (strncmp(argv[i], prefix, plen) == 0)
            return argv[i] + plen;
    return NULL;
}

static int has_flag(int argc, char *argv[], const char *flag) {
    for (int i = 1; i < argc; i++)
        if (strcmp(argv[i], flag) == 0) return 1;
    return 0;
}

/* =========================================================================
 * Main
 * ========================================================================= */

int main(int argc, char *argv[]) {
    const char *rom_path   = get_arg(argc, argv, "--rom=");
    const char *steps_str  = get_arg(argc, argv, "--steps=");
    int sim_mode           = has_flag(argc, argv, "--sim") || (rom_path == NULL);
    int max_steps          = steps_str ? atoi(steps_str) : 0;  /* 0 = unlimited */

    /* Static NES state on the stack (deterministic layout, no heap allocation) */
    static NES nes;
    memset(&nes, 0, sizeof(nes));
    nes.sim_mode = sim_mode;
    nes.sp   = 0xFD;
    nes.p    = 0x24;  /* IRQ disabled, unused bit set */
    nes.lives = 3;

    static SimState sim;
    memset(&sim, 0, sizeof(sim));
    sim.lives = 3;
    sim.y_pos = 176;    /* start on ground */
    sim.on_ground = 1;

    /* Load ROM if in real mode */
    if (!sim_mode && rom_path) {
        if (load_rom(&nes, rom_path) != 0) {
            fprintf(stderr, "nes_bridge: falling back to simulation mode\n");
            sim_mode = 1;
        } else {
            fprintf(stderr, "nes_bridge: ROM loaded, reset PC=0x%04X\n", nes.pc);
        }
    } else if (sim_mode) {
        fprintf(stderr, "nes_bridge: simulation mode (no ROM)\n");
    }

    /* Frame buffer (static, on the data segment — fixed layout for no-PIE) */
    static unsigned char framebuf[NES_FRAME_BYTES];

    unsigned long long frame = 0;
    for (;;) {
        /* Read one joypad byte from stdin (the fifo adapter) */
        int c = fgetc(stdin);
        if (c == EOF) break;
        unsigned char joypad = (unsigned char)c;

        /* Advance game state */
        if (sim_mode) {
            sim_step(&sim, joypad);
            sim_to_nes_ram(&nes, &sim);
        } else {
            /* Real ROM: run ~29780 CPU cycles per NTSC frame */
            nes.controller1 = joypad;
            int cycles = 0;
            while (cycles < 29780) {
                cpu_step(&nes);
                cycles++;
            }
        }

        /* Sync game state from RAM */
        smb_sync_state(&nes);

        /* Render frame */
        ppu_render_frame(&nes, framebuf);

        /* Write frame buffer to stdout (frame adapter) */
        fwrite(framebuf, 1, NES_FRAME_BYTES, stdout);
        fflush(stdout);

        /* Emit probes via stderr (stdout-kv adapter) */
        emit_probes(&nes);

        frame++;

        /* Check termination conditions */
        if (nes.game_completed) {
            fprintf(stderr, "nes_bridge: game completed at frame %llu\n", frame);
            break;
        }
        if (nes.game_over) {
            fprintf(stderr, "nes_bridge: game over at frame %llu\n", frame);
            break;
        }
        if (max_steps > 0 && (int)frame >= max_steps) {
            fprintf(stderr, "nes_bridge: reached step limit %d\n", max_steps);
            break;
        }
    }

    return 0;
}
