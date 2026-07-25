.intel_syntax noprefix
.code64
.global _start
_start:
    mov dx, 0x3f8
    mov al, 0x53
    out dx, al
    .byte 0x0F, 0xC7, 0xF8      # rdseed eax -- build.py rewrites this to UD2 + NOP
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
