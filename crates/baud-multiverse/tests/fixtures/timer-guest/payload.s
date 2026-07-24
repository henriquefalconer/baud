.intel_syntax noprefix
.code64
.global _start

.equ COM1, 0x3f8
.equ VECTOR, 0x30
# baud-multiverse always sets RIP = kernel_load + KERNEL_64BIT_ENTRY_OFFSET
# (layout::KERNEL_LOAD_ADDR = 0x00200000, layout::KERNEL_64BIT_ENTRY_OFFSET = 0x200) -- the
# flat binary below is linked at 0 (`ld -Ttext=0`) but actually runs from this address, so any
# absolute (non-RIP-relative) data reference -- an IDT gate's target offset, IDTR's base -- must
# add this constant by hand. `lidt [rip + idtr]` itself needs no such adjustment: RIP-relative
# instruction operands are base-independent by construction.
.equ RUNTIME_BASE, 0x00200200

_start:
    lidt [rip + idtr]
    sti
    mov ecx, 20000
outer:
    mov ebx, 16
inner:
    dec ebx
    jnz inner
    # A harmless forced VM exit every 16 branches (port 0x80 is outside every real device's PIO
    # window -- COM1/CMOS/tape -- so `DeviceBus`'s open-bus fallback silently absorbs the write,
    # no side effect at all). Needed so `LinuxPmuStepper::run_until_exit` gets a chance to notice
    # its armed counter has crossed the target by polling directly -- real hardware finding: a PMU
    # overflow occurring purely inside guest-mode execution with no other exit in a long stretch
    # is never recognized as a signal on this project's own nested-virtualized dev host, so this
    # fixture cannot rely on one at all (see `LinuxPmuStepper`'s module doc, todo.md §14). This
    # interval must stay smaller than `boundary::MARGIN` (64): the "early exit" is only ever
    # detected at one of these forced traps, so a coarser interval would let it overshoot straight
    # past the target before `inject_at`'s single-step loop ever gets a chance to run, silently
    # defeating the whole arm-early-then-single-step mechanism (found for real: with an interval of
    # 1000, `timer_tick_lands_at_identical_instruction` always landed 0 single-steps in and varied
    # by a few branches run to run, purely from unfiltered host-side counter jitter accumulated
    # over many more polling iterations).
    out 0x80, al
    dec ecx
    jnz outer
2:
    hlt
    jmp 2b

# The injected vector's handler: write one marker byte to COM1, then resume the interrupted
# loop exactly where it left off (`iretq` restores RIP/CS/RFLAGS/RSP/SS from the frame the CPU
# itself pushed on delivery).
isr:
    push rax
    push rdx
    mov dx, COM1
    mov al, 'T'
    out dx, al
    pop rdx
    pop rax
    iretq

# A 64-bit IDT with real entries only up to VECTOR (0x30): every lower vector stays a zeroed,
# not-present gate (never triggered by this fixture -- no faults are expected), VECTOR points at
# `isr` above. GAS/ld can resolve a simple `symbol + constant` relocation (used for `idtr` below)
# but not a bitwise mask/shift of one (needed to split isr's 64-bit address across this gate's
# three offset fields) -- these three offset fields are left as `0` placeholders and patched
# directly into the assembled flat binary by build.py, which reads `isr`'s real address from the
# linked ELF's symbol table. See BUILD.md for the gate byte layout this hand-encodes.
.align 16
idt_start:
    .fill (VECTOR * 16), 1, 0
    .word 0                                    # offset[15:0]  -- patched by build.py
    .word 0x08                                 # selector -- layout::GDT_CODE_SELECTOR
    .byte 0                                     # IST = 0 (no interrupt stack table in use)
    .byte 0x8e                                  # P=1 DPL=00 S=0 Type=1110 (64-bit interrupt gate)
    .word 0                                    # offset[31:16] -- patched by build.py
    .long 0                                    # offset[63:32] -- patched by build.py
    .long 0                                      # reserved
idt_end:

idtr:
    .word (idt_end - idt_start - 1)
    .quad (idt_start + RUNTIME_BASE)
