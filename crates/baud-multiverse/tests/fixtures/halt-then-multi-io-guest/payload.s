.intel_syntax noprefix
.code64
.global _start

.equ COM1, 0x3f8
.equ VECTOR, 0x30
# baud-multiverse always sets RIP = kernel_load + KERNEL_64BIT_ENTRY_OFFSET
# (layout::KERNEL_LOAD_ADDR = 0x00200000, layout::KERNEL_64BIT_ENTRY_OFFSET = 0x200) -- the
# flat binary below is linked at 0 (`ld -Ttext=0`) but actually runs from this address, so any
# absolute (non-RIP-relative) data reference -- an IDT gate's target offset, IDTR's base -- must
# add this constant by hand, same convention as ../halt-then-spin-guest/payload.s.
.equ RUNTIME_BASE, 0x00200200

_start:
    lidt [rip + idtr]
    sti
    hlt
# Reached only via the injected timer interrupt's `iretq` (which restores RIP to right after
# `hlt`), same as ../halt-then-spin-guest/payload.s -- but instead of falling straight into a
# zero-exit spin, this fixture performs three separate `out` writes first, each one its own VM
# exit, before spinning forever. This is what exercises the resume-past-halt burst loop's
# per-exit device-servicing check (crates/baud-multiverse/src/linux/mod.rs, todo.md §14.2 H9
# items 20/21/22's flagged gap): a real device's completion arriving *between* two of these three
# raw exits, not just once per periodic tick.
    mov dx, COM1
    mov al, 'A'
    out dx, al
    mov al, 'B'
    out dx, al
    mov al, 'C'
    out dx, al
final:
    jmp final

# The injected vector's handler: does nothing but resume execution at the point `hlt` was
# interrupted (i.e. right after it, at the first `mov dx, COM1` above). Unlike
# ../halt-then-spin-guest/payload.s's ISR, this one writes no console marker itself -- all three
# markers come from the resumed main flow instead, so each is a distinct, individually observable
# VM exit inside the burst loop.
isr:
    iretq

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
