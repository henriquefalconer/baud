.intel_syntax noprefix
.code64
.global _start
_start:
    mov dx, 0x3f8
    mov al, 0x46            # 'F' marker, echoed to COM1 before anything else
    out dx, al

    mov dx, 0x0500           # tape device DATA port (baud-multiverse::tape_bus base 0x0500)
    mov al, 2                 # FRAME header byte 0: pixel format tag 2 = Indexed8
    out dx, al
    mov al, 2                 # width (u32 LE) = 2
    out dx, al
    mov al, 0
    out dx, al
    out dx, al
    out dx, al
    mov al, 2                 # height (u32 LE) = 2
    out dx, al
    mov al, 0
    out dx, al
    out dx, al
    out dx, al
    mov al, 10                # 4 raw indexed8 pixel bytes
    out dx, al
    mov al, 20
    out dx, al
    mov al, 30
    out dx, al
    mov al, 40
    out dx, al

    mov dx, 0x0508            # tape device CONTROL port: finalize as FRAME (opcode 5)
    mov al, 5
    out dx, al

2:
    hlt
    jmp 2b
