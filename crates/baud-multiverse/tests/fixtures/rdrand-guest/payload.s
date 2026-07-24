.intel_syntax noprefix
.code64
.global _start
_start:
    mov dx, 0x3f8
    mov al, 0x58
    out dx, al
    rdrand eax
    mov ecx, 4
1:
    mov dx, 0x3f8
    out dx, al
    shr eax, 8
    dec ecx
    jnz 1b
2:
    hlt
    jmp 2b
