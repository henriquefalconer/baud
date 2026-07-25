.intel_syntax noprefix
.code64
.global _start
_start:
    mov dx, 0x3f8
    mov al, 0x54
    out dx, al
    rdtsc
    shl rdx, 32
    or rax, rdx
    mov ecx, 8
1:
    mov dx, 0x3f8
    out dx, al
    shr rax, 8
    dec ecx
    jnz 1b
2:
    hlt
    jmp 2b
