.intel_syntax noprefix
.code64
.global _start
_start:
prompt:
    mov dx, 0x3f8
    mov al, '$'
    out dx, al
    mov al, ' '
    out dx, al
read_loop:
    mov dx, 0x3fd
    in al, dx
    test al, 1
    jz read_loop
    mov dx, 0x3f8
    in al, dx
    cmp al, 0x0d
    je got_cr
    mov dx, 0x3f8
    out dx, al
    jmp read_loop
got_cr:
    mov al, 0x0a
    out dx, al
    jmp prompt
