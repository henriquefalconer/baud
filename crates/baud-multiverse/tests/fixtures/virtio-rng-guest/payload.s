.intel_syntax noprefix
.code64
.global _start

.equ COM1, 0x3f8
.equ VECTOR, 0x31
# baud-multiverse always sets RIP = kernel_load + KERNEL_64BIT_ENTRY_OFFSET
# (layout::KERNEL_LOAD_ADDR = 0x00200000, layout::KERNEL_64BIT_ENTRY_OFFSET = 0x200) -- same
# runtime-base convention as ../timer-guest/payload.s; see its own comment for why absolute (non
# RIP-relative) data references need this added by hand.
.equ RUNTIME_BASE, 0x00200200

# The virtio-mmio v2 transport window (layout::VIRTIO_MMIO_RNG_BASE) and the register offsets
# `virtio_mmio.rs` defines (spec 1.1 §4.2.2 Table 4.1) -- this fixture is a real (if minimal)
# virtio-rng driver, not a test harness shortcut: every register write below is exactly what an
# unmodified kernel's virtio_mmio probe + virtio_rng probe + one hwrng_fillfn request would issue.
.equ VIRTIO_BASE, 0xd0000000
.equ REG_DEVICE_FEATURES, 0x010
.equ REG_DEVICE_FEATURES_SEL, 0x014
.equ REG_DRIVER_FEATURES, 0x020
.equ REG_DRIVER_FEATURES_SEL, 0x024
.equ REG_QUEUE_SEL, 0x030
.equ REG_QUEUE_NUM_MAX, 0x034
.equ REG_QUEUE_NUM, 0x038
.equ REG_QUEUE_READY, 0x044
.equ REG_QUEUE_NOTIFY, 0x050
.equ REG_STATUS, 0x070
.equ REG_QUEUE_DESC_LOW, 0x080
.equ REG_QUEUE_DESC_HIGH, 0x084
.equ REG_QUEUE_DRIVER_LOW, 0x090
.equ REG_QUEUE_DRIVER_HIGH, 0x094
.equ REG_QUEUE_DEVICE_LOW, 0x0a0
.equ REG_QUEUE_DEVICE_HIGH, 0x0a4

# Status-register values (spec 1.1 §2.1), pre-combined since GAS's Intel-mode immediate-operand
# parser is not exercised elsewhere in this project's fixtures for bitwise-OR'd symbols -- plain
# literals, one per driver-probe stage, are unambiguous:
#   1  = ACKNOWLEDGE
#   3  = ACKNOWLEDGE | DRIVER
#   11 = ACKNOWLEDGE | DRIVER | FEATURES_OK
#   15 = ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK
.equ STATUS_ACK, 1
.equ STATUS_ACK_DRIVER, 3
.equ STATUS_ACK_DRIVER_FEATURES_OK, 11
.equ STATUS_ACK_DRIVER_FEATURES_OK_DRIVER_OK, 15

# Guest-RAM addresses for the queue's three rings plus the one data buffer this fixture posts --
# comfortably clear of the loaded flat binary (RUNTIME_BASE, a few hundred bytes) and of every
# other fixed low-memory structure baud writes (all below 0x100000).
.equ DESC_ADDR, 0x00400000
.equ AVAIL_ADDR, 0x00401000
.equ USED_ADDR, 0x00402000
.equ DATA_ADDR, 0x00403000
.equ DATA_LEN, 64

_start:
    lidt [rip + idtr]
    sti

    mov edi, VIRTIO_BASE

    mov dword ptr [rdi + REG_STATUS], STATUS_ACK
    mov dword ptr [rdi + REG_STATUS], STATUS_ACK_DRIVER

    mov dword ptr [rdi + REG_DEVICE_FEATURES_SEL], 1   # word 1 (bits 32..64): carries VIRTIO_F_VERSION_1
    mov eax, [rdi + REG_DEVICE_FEATURES]
    mov dword ptr [rdi + REG_DRIVER_FEATURES_SEL], 1
    mov [rdi + REG_DRIVER_FEATURES], eax               # accept whatever was offered, verbatim
    mov dword ptr [rdi + REG_STATUS], STATUS_ACK_DRIVER_FEATURES_OK

    mov dword ptr [rdi + REG_QUEUE_SEL], 0
    mov eax, [rdi + REG_QUEUE_NUM_MAX]
    mov [rdi + REG_QUEUE_NUM], eax
    mov dword ptr [rdi + REG_QUEUE_DESC_LOW], DESC_ADDR
    mov dword ptr [rdi + REG_QUEUE_DESC_HIGH], 0
    mov dword ptr [rdi + REG_QUEUE_DRIVER_LOW], AVAIL_ADDR
    mov dword ptr [rdi + REG_QUEUE_DRIVER_HIGH], 0
    mov dword ptr [rdi + REG_QUEUE_DEVICE_LOW], USED_ADDR
    mov dword ptr [rdi + REG_QUEUE_DEVICE_HIGH], 0
    mov dword ptr [rdi + REG_QUEUE_READY], 1
    mov dword ptr [rdi + REG_STATUS], STATUS_ACK_DRIVER_FEATURES_OK_DRIVER_OK

    # One writable descriptor pointing at DATA_ADDR/DATA_LEN (spec 1.1 §2.6.5: addr:u64, len:u32,
    # flags:u16, next:u16 -- VIRTQ_DESC_F_WRITE = 2, no chaining so next is unused).
    mov rsi, DESC_ADDR
    mov rax, DATA_ADDR
    mov [rsi], rax
    mov dword ptr [rsi + 8], DATA_LEN
    mov word ptr [rsi + 12], 2
    mov word ptr [rsi + 14], 0

    # The avail ring (spec 1.1 §2.6.6): flags=0, idx=1 (one entry posted), ring[0]=0 (descriptor
    # chain head index).
    mov rbx, AVAIL_ADDR
    mov word ptr [rbx], 0
    mov word ptr [rbx + 2], 1
    mov word ptr [rbx + 4], 0

    mov dword ptr [rdi + REG_QUEUE_NOTIFY], 0

    # Busy-loop long enough for the test harness to observe the QueueNotify write (one exit at a
    # time via `Multiverse::step_exit`), service the ring, and inject the interrupt mid-loop --
    # same forced-VM-exit-every-16-branches shape as ../timer-guest/payload.s, and for the same
    # reason (`LinuxPmuStepper::run_until_exit` only ever polls at one of these).
    mov ecx, 20000
outer:
    mov ebx, 16
inner:
    dec ebx
    jnz inner
    out 0x80, al
    dec ecx
    jnz outer
2:
    hlt
    jmp 2b

# The injected vector's handler: write a fixed marker byte, then the first byte of the buffer the
# device's used-ring completion filled -- proving both that the interrupt landed *and* that the
# guest's own ISR can see the real tape-seeded entropy byte `service_virtio_rng` wrote, not just
# that some interrupt fired.
isr:
    push rax
    push rdx
    mov dx, COM1
    mov al, 'R'
    out dx, al
    mov rax, DATA_ADDR
    mov al, [rax]
    out dx, al
    pop rdx
    pop rax
    iretq

# A 64-bit IDT with a real entry only at VECTOR (0x31); see ../timer-guest/payload.s's comment for
# why the three offset fields are patched by build.py rather than resolved by GAS/ld directly.
.align 16
idt_start:
    .fill (VECTOR * 16), 1, 0
    .word 0                                    # offset[15:0]  -- patched by build.py
    .word 0x08                                 # selector -- layout::GDT_CODE_SELECTOR
    .byte 0                                     # IST = 0
    .byte 0x8e                                  # P=1 DPL=00 S=0 Type=1110 (64-bit interrupt gate)
    .word 0                                    # offset[31:16] -- patched by build.py
    .long 0                                    # offset[63:32] -- patched by build.py
    .long 0                                      # reserved
idt_end:

idtr:
    .word (idt_end - idt_start - 1)
    .quad (idt_start + RUNTIME_BASE)
