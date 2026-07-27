.intel_syntax noprefix
.code64
.global _start

.equ COM1, 0x3f8
.equ VECTOR, 0x30
# See ../timer-guest/payload.s's own comment on this constant: baud-multiverse always sets
# RIP = kernel_load + KERNEL_64BIT_ENTRY_OFFSET (layout::KERNEL_LOAD_ADDR = 0x00200000,
# layout::KERNEL_64BIT_ENTRY_OFFSET = 0x200), and this flat binary is linked at 0
# (`ld -Ttext=0`), so any absolute (non-RIP-relative) reference -- only the IDT gate's offset
# fields here, patched by build.py -- must add this constant by hand.
.equ RUNTIME_BASE, 0x00200200

_start:
    lidt [rip + idtr]
    sti
loop:
    # Immediately idle -- unlike timer-guest, no busy loop at all. Models a real kernel's idle
    # loop calling safe_halt() the instant nothing is runnable: `run_until_console_pattern_with_
    # periodic_timer` must wake this purely via directly-staged timer interrupts, never via the
    # arm-early-then-single-step `inject_at` engine (which cannot deliver to an already-halted
    # vCPU -- it reports `Halted` immediately without ever calling `PmuStepper::inject`).
    hlt
    jmp loop

# The injected timer vector's handler: count how many times it has woken this guest. The first
# WAKES_BEFORE_MESSAGE - 1 wakes do nothing observable (models a real idle guest halting
# repeatedly for unrelated reasons, e.g. blocked on disk I/O, before the target console text ever
# appears); only once the threshold is reached does it emit the target message, one byte per
# `out`, each of which forces its own VM exit -- so a caller resuming past a halt must keep
# draining exits after delivery, not assume a single `step_exit` suffices in general.
.equ WAKES_BEFORE_MESSAGE, 5

isr:
    push rax
    push rdx
    push rsi
    push rdi
    lea rsi, [rip + wake_count]
    mov eax, [rsi]
    inc eax
    mov [rsi], eax
    cmp eax, WAKES_BEFORE_MESSAGE
    jl isr_done
    lea rsi, [rip + msg]
    lea rdi, [rip + msg_end]
write_loop:
    cmp rsi, rdi
    jge isr_done
    mov al, [rsi]
    mov dx, COM1
    out dx, al
    inc rsi
    jmp write_loop
isr_done:
    pop rdi
    pop rsi
    pop rdx
    pop rax
    iretq

wake_count:
    .long 0
msg:
    .ascii "ubuntu login:"
msg_end:

# A 64-bit IDT with a real entry only at VECTOR (0x30); every other vector stays a zeroed,
# not-present gate (never triggered -- no faults are expected). Same layout as
# ../timer-guest/payload.s; see that file's comment for why the three offset fields are left as
# `0` placeholders here and patched directly into the assembled flat binary by build.py.
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
