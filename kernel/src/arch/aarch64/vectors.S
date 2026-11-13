/*
 * SketchOS AArch64 exception vectors and context switch.
 *
 * The table is 2 KiB aligned as required by VBAR_EL1/VBAR_EL2/VBAR_EL3.
 * Every entry builds the same 816-byte trap frame:
 *
 *     0   x0 .. x30        (31 * 8 = 248 bytes)
 *   248   ELR_ELx
 *   256   SPSR_ELx
 *   264   ESR_ELx
 *   272   FAR_ELx
 *   280   SP_EL0
 *   288   q0 .. q31        (32 * 16 = 512 bytes)
 *   800   FPSR
 *   808   FPCR
 *
 * Synchronous exceptions, FIQs and SErrors reach `exception_handler`, which
 * decides whether the fault is recoverable (a kernel probe or a lower-EL
 * fault with a hook) and returns the frame to resume. Everything else halts
 * inside the handler. IRQs return through `restore_frame`, which is also used
 * by `scheduler_enter` to start the first task. The scheduler swaps the frame
 * pointer returned by `irq_handler`, so a timer tick can switch kernel threads
 * without returning to the interrupted one.
 */

    .section ".text.exception_vectors", "ax"
    .align 11
    .global exception_vectors
    .type exception_vectors, %function

.macro vector_entry index, target
    .align 7
    sub     sp, sp, #816
    stp     x0, x1, [sp, #0]
    mov     x0, #\index
    b       \target
.endm

exception_vectors:
    /* Current EL with SP_EL0: synchronous, IRQ, FIQ, SError. */
    vector_entry 0, exception_common
    vector_entry 1, irq_common
    vector_entry 2, exception_common
    vector_entry 3, exception_common

    /* Current EL with SP_ELx: synchronous, IRQ, FIQ, SError. */
    vector_entry 4, exception_common
    vector_entry 5, irq_common
    vector_entry 6, exception_common
    vector_entry 7, exception_common

    /* Lower EL using AArch64: synchronous, IRQ, FIQ, SError. */
    vector_entry 8, exception_common
    vector_entry 9, irq_common
    vector_entry 10, exception_common
    vector_entry 11, exception_common

    /* Lower EL using AArch32: synchronous, IRQ, FIQ, SError. */
    vector_entry 12, exception_common
    vector_entry 13, irq_common
    vector_entry 14, exception_common
    vector_entry 15, exception_common

    .size exception_vectors, . - exception_vectors

/* Save x2..x30 and the FP/SIMD state. x0/x1 and the vector index are already
 * on the frame. */
.macro save_common
    stp     x2, x3, [sp, #16]
    stp     x4, x5, [sp, #32]
    stp     x6, x7, [sp, #48]
    stp     x8, x9, [sp, #64]
    stp     x10, x11, [sp, #80]
    stp     x12, x13, [sp, #96]
    stp     x14, x15, [sp, #112]
    stp     x16, x17, [sp, #128]
    stp     x18, x19, [sp, #144]
    stp     x20, x21, [sp, #160]
    stp     x22, x23, [sp, #176]
    stp     x24, x25, [sp, #192]
    stp     x26, x27, [sp, #208]
    stp     x28, x29, [sp, #224]
    str     x30, [sp, #240]

    stp     q0, q1, [sp, #288]
    stp     q2, q3, [sp, #320]
    stp     q4, q5, [sp, #352]
    stp     q6, q7, [sp, #384]
    stp     q8, q9, [sp, #416]
    stp     q10, q11, [sp, #448]
    stp     q12, q13, [sp, #480]
    stp     q14, q15, [sp, #512]
    stp     q16, q17, [sp, #544]
    stp     q18, q19, [sp, #576]
    stp     q20, q21, [sp, #608]
    stp     q22, q23, [sp, #640]
    stp     q24, q25, [sp, #672]
    stp     q26, q27, [sp, #704]
    stp     q28, q29, [sp, #736]
    stp     q30, q31, [sp, #768]

    mrs     x2, fpsr
    str     x2, [sp, #800]
    mrs     x2, fpcr
    str     x2, [sp, #808]
.endm

/* Capture ELR/SPSR/ESR/FAR from the exception-level bank that owns the
 * exception. The kernel runs at EL1, but the EL2/EL3 cases keep the fatal
 * diagnostic useful if firmware ever enters higher. */
.macro save_system_registers
    mrs     x2, CurrentEL
    and     x2, x2, #0xc
    cmp     x2, #0x8
    b.eq    2f
    cmp     x2, #0xc
    b.eq    3f

    mrs     x2, elr_el1
    mrs     x3, spsr_el1
    mrs     x4, esr_el1
    mrs     x5, far_el1
    b       4f

2:
    mrs     x2, elr_el2
    mrs     x3, spsr_el2
    mrs     x4, esr_el2
    mrs     x5, far_el2
    b       4f

3:
    mrs     x2, elr_el3
    mrs     x3, spsr_el3
    mrs     x4, esr_el3
    mrs     x5, far_el3

4:
    stp     x2, x3, [sp, #248]
    stp     x4, x5, [sp, #264]

    /* The user stack pointer is banked in SP_EL0. Saving it here lets a
     * kernel-to-user switch restore a thread's EL0 stack even when the
     * exception itself came from EL1. */
    mrs     x2, sp_el0
    str     x2, [sp, #280]
.endm

exception_common:
    save_common
    save_system_registers
    mov     x1, sp
    bl      exception_handler
    /* x0 is the frame to resume: the interrupted one, or one whose ELR the
     * handler rewrote to a probe resume label. */
    mov     sp, x0
    b       restore_frame

irq_common:
    save_common
    save_system_registers
    mov     x1, sp
    bl      irq_handler
    /* x0 is the frame to resume: the current one or another task's. */
    mov     sp, x0
    b       restore_frame

/*
 * Enter the first task. x0 points at a frame built by the scheduler.
 */
    .global scheduler_enter
    .type scheduler_enter, %function
scheduler_enter:
    mov     sp, x0
    b       restore_frame
    .size scheduler_enter, . - scheduler_enter

restore_frame:
    ldr     x2, [sp, #248]
    ldr     x3, [sp, #256]
    msr     elr_el1, x2
    msr     spsr_el1, x3
    ldr     x2, [sp, #280]
    msr     sp_el0, x2

    ldp     q0, q1, [sp, #288]
    ldp     q2, q3, [sp, #320]
    ldp     q4, q5, [sp, #352]
    ldp     q6, q7, [sp, #384]
    ldp     q8, q9, [sp, #416]
    ldp     q10, q11, [sp, #448]
    ldp     q12, q13, [sp, #480]
    ldp     q14, q15, [sp, #512]
    ldp     q16, q17, [sp, #544]
    ldp     q18, q19, [sp, #576]
    ldp     q20, q21, [sp, #608]
    ldp     q22, q23, [sp, #640]
    ldp     q24, q25, [sp, #672]
    ldp     q26, q27, [sp, #704]
    ldp     q28, q29, [sp, #736]
    ldp     q30, q31, [sp, #768]

    ldr     x2, [sp, #800]
    msr     fpsr, x2
    ldr     x2, [sp, #808]
    msr     fpcr, x2

    ldp     x2, x3, [sp, #16]
    ldp     x4, x5, [sp, #32]
    ldp     x6, x7, [sp, #48]
    ldp     x8, x9, [sp, #64]
    ldp     x10, x11, [sp, #80]
    ldp     x12, x13, [sp, #96]
    ldp     x14, x15, [sp, #112]
    ldp     x16, x17, [sp, #128]
    ldp     x18, x19, [sp, #144]
    ldp     x20, x21, [sp, #160]
    ldp     x22, x23, [sp, #176]
    ldp     x24, x25, [sp, #192]
    ldp     x26, x27, [sp, #208]
    ldp     x28, x29, [sp, #224]
    ldr     x30, [sp, #240]
    ldp     x0, x1, [sp, #0]
    add     sp, sp, #816
    eret

    .size irq_common, . - irq_common
