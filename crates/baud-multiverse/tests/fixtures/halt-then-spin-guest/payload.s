.intel_syntax noprefix
.code64
.global _start

.equ COM1, 0x3f8
.equ VECTOR, 0x30
# baud-multiverse always sets RIP = kernel_load + KERNEL_64BIT_ENTRY_OFFSET
# (layout::KERNEL_LOAD_ADDR = 0x00200000, layout::KERNEL_64BIT_ENTRY_OFFSET = 0x200) -- the
# flat binary below is linked at 0 (`ld -Ttext=0`) but actually runs from this address, so any
# absolute (non-RIP-relative) data reference -- an IDT gate's target offset, IDTR's base -- must
# add this constant by hand, same convention as ../timer-guest/payload.s.
.equ RUNTIME_BASE, 0x00200200

_start:
    lidt [rip + idtr]
    sti
    hlt
# Reached only via the injected timer interrupt's `iretq` (which restores RIP to right after
# `hlt`, the point execution was interrupted at) -- not via any code path within this payload
# itself. Zero conditional branches, zero I/O, zero HLT: once entered, this retires no further VM
# exit, ever, exactly like ../spin-guest/payload.s's `1: jmp 1b`, but reached *after* one real
# `Hlt` exit and one delivered interrupt instead of from a cold start.
spin:
    jmp spin

# The injected vector's handler: write one marker byte to COM1 (so the burst loop that delivered
# this interrupt sees at least one ordinary VM exit before the guest falls into `spin` above and
# stops exiting altogether), then resume execution at the point `hlt` was interrupted, i.e. `spin`.
isr:
    push rax
    push rdx
    mov dx, COM1
    mov al, 'T'
    out dx, al
    pop rdx
    pop rax
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
