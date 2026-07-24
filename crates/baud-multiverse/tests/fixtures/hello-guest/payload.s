.intel_syntax noprefix
.code64
.global _start
_start:
    lea rsi, [rip + msg]
    mov ecx, 17
1:
    mov al, [rsi]
    mov dx, 0x3f8
    out dx, al
    inc rsi
    dec rcx
    jnz 1b
2:
    hlt
    jmp 2b
msg:
    .ascii "BAUD_HELLO_GUEST\n"
