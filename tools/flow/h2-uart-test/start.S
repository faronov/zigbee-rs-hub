    .section .text.entry, "ax"
    .globl _start
_start:
    la sp, _stack_top
    call main_loop
1:  j 1b
