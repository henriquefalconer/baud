.intel_syntax noprefix
.code64
.global _start
_start:
    mov ecx, 4
1:
    mov dx, 0x0500
    in al, dx
    mov dx, 0x3f8
    out dx, al
    dec ecx
    jnz 1b
2:
    hlt
    jmp 2b
