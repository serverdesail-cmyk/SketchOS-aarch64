/*
 * SketchOS AArch64 kernel entry.
 *
 * Contract with the UEFI loader:
 *   x0 = physical address of sketchos_bootinfo::BootInfo
 *   MMU enabled under the firmware identity map, caches enabled
 *   execution at EL1 or EL2
 *
 * The loader has already left Boot Services. This code must not call firmware.
 * It has also built two translation roots and described them in the boot
 * record:
 *
 *   TTBR0  bootstrap identity map (kernel image + boot record)
 *   TTBR1  higher-half kernel image plus the linear physical map
 *
 * The kernel image is linked as a position-independent ET_DYN image at virtual
 * address zero. The loader chooses the physical load address and applies the
 * R_AARCH64_RELATIVE relocations. Runtime virtual addresses are
 * `KERNEL_VA_BASE + physical`, so the physical-to-virtual bias is
 * `kernel_va_start - kernel_phys_start`.
 *
 * Execution starts physically. This file clears .bss, installs the loader's
 * roots, enables the EL1 MMU and only then branches into the higher half.
 */

    .equ BOOT_KERNEL_PHYS_START, 56
    .equ BOOT_TTBR0_ROOT,        208
    .equ BOOT_TTBR1_ROOT,        216
    .equ BOOT_TCR,               224
    .equ BOOT_MAIR,              232
    .equ BOOT_KERNEL_VA_START,   240

    /* Linux arm64's baseline SCTLR_EL1 RES1 bits. Do not inherit an unknown
     * EL1 control-register value when UEFI entered at EL2. */
    .equ SCTLR_EL1_RES1,         0x30500800

    .section ".text._start", "ax"
    .global _start
    .type _start, %function

_start:
    /* Mask all asynchronous exceptions (debug, SError, IRQ, FIQ). */
    msr     daifset, #0xf

    /* x20 = boot record, x19 = entry exception level. */
    mov     x20, x0
    mrs     x19, CurrentEL
    lsr     x19, x19, #2
    and     x19, x19, #3

    /* Load the translation plan before any register is reused. */
    ldr     x21, [x20, #BOOT_TTBR0_ROOT]
    ldr     x22, [x20, #BOOT_TTBR1_ROOT]
    ldr     x23, [x20, #BOOT_TCR]
    ldr     x24, [x20, #BOOT_MAIR]
    ldr     x25, [x20, #BOOT_KERNEL_VA_START]
    ldr     x26, [x20, #BOOT_KERNEL_PHYS_START]
    sub     x26, x25, x26           /* physical -> virtual bias */

    /* Clear .bss, including the early stack region and the kernel's page-table
     * pool. Every reference is physical here. */
    adrp    x2, __bss_start
    add     x2, x2, :lo12:__bss_start
    adrp    x3, __bss_end
    add     x3, x3, :lo12:__bss_end
1:
    cmp     x2, x3
    b.hs    2f
    str     xzr, [x2], #8
    b       1b

2:
    /*
     * The BSS clear above may leave dirty cache lines. Clean+invalidate the
     * identity-mapped physical range to PoC before SCTLR.C is cleared, or the
     * zeroing can be lost when the data cache is disabled.
     */
    mrs     x4, ctr_el0
    ubfx    x4, x4, #16, #4
    mov     x5, #4
    lsl     x5, x5, x4
    adrp    x2, __bss_start
    add     x2, x2, :lo12:__bss_start
    adrp    x3, __bss_end
    add     x3, x3, :lo12:__bss_end
    sub     x6, x5, #1
    bic     x2, x2, x6
3:
    cmp     x2, x3
    b.hs    4f
    dc      civac, x2
    add     x2, x2, x5
    b       3b
4:
    dsb     sy

    cmp     x19, #2
    b.eq    el2_setup
    cmp     x19, #1
    b.eq    el1_install_enter

    /* EL3 would need the secure monitor's cooperation and EL0 is impossible
     * for a kernel entry. Park rather than run with the wrong assumptions. */
    b       park

/*
 * Drop from EL2 to EL1 and install the loader's translation regime.
 *
 * Firmware may enter the kernel at EL2 (for example, a UEFI implementation
 * that keeps EL2 for itself). The rest of the kernel uses the EL1 system
 * registers for the MMU, GIC CPU interface, timer and exception vectors.
 *
 * x20 = BootInfo physical address (preserved)
 * x19 = entry exception level (preserved)
 * x21..x26 = translation plan and bias
 */
el2_setup:
    /* Install physical exception vectors before changing EL1 state. The
     * identity map covers the kernel image until the higher-half switch. */
    adrp    x9, exception_vectors
    add     x9, x9, :lo12:exception_vectors
    msr     vbar_el2, x9
    msr     vbar_el1, x9
    isb

    /* Give EL1 deterministic FP/SIMD access before any vector can save Q regs. */
    mov     x9, #(3 << 20)
    msr     cpacr_el1, x9
    isb

    /* HCR_EL2: EL1 is AArch64, no VHE, and no traps from EL1 to EL2.
     * Clearing TGE means EL1 is a normal guest OS rather than the EL2 host.
     * Clearing TSC lets PSCI SMC calls reach EL3 directly. */
    mov     x9, #(1 << 31)          /* HCR_RW */
    msr     hcr_el2, x9
    isb

    /* Allow EL1 to read the physical counter and program the physical timer. */
    mov     x9, #3                  /* EL1PCTEN | EL1PCEN */
    msr     cnthctl_el2, x9
    msr     cntvoff_el2, xzr
    isb

    /* Do not trap FP/SIMD, SVE or other EL1 accesses to EL2. */
    mrs     x9, cptr_el2
    mov     x10, #(1 << 8)          /* TZ */
    bic     x9, x9, x10
    mov     x10, #(1 << 10)         /* TFP */
    bic     x9, x9, x10
    mov     x10, #(1 << 12)         /* TCPAC */
    bic     x9, x9, x10
    mov     x10, #(1 << 20)         /* TTA */
    bic     x9, x9, x10
    msr     cptr_el2, x9
    isb

    /* If the GIC implements the system-register interface, let EL1 use it.
     * ID_AA64PFR0_EL1.GIC is zero on a machine without that interface. */
    mrs     x9, id_aa64pfr0_el1
    ubfx    x9, x9, #24, #4
    cbz     x9, 3f
    mov     x9, #0xf                /* SRE | DFB | DIB | Enable */
    msr     ICC_SRE_EL2, x9
    isb
3:
    /* Install the loader's EL1 translation regime and return to EL1h. The
     * first EL1 instruction fetch uses EL1 translation, so the roots and TCR
     * must be live before `eret`. */
    bl      el1_install

    adrp    x9, el1_entry
    add     x9, x9, :lo12:el1_entry
    msr     elr_el2, x9
    mov     x9, #0x3c5              /* DAIF masked, EL1h, AArch64 */
    msr     spsr_el2, x9
    adrp    x9, __stack_top
    add     x9, x9, :lo12:__stack_top
    msr     sp_el1, x9
    isb
    eret

/*
 * Install the loader's EL1 roots and enable the MMU/caches. Safe at EL1 or,
 * with HCR_EL2 not trapping them, from EL2.
 *
 * Clobbers x9/x10/x11 only.
 */
el1_install:
    /*
     * Install an architectural, little-endian EL1 baseline instead of
     * read-modify-writing SCTLR_EL1. When UEFI runs at EL2, EL1's register
     * state is not guaranteed to have been initialized by firmware.
     * Turn it off before replacing TTBR/TCR/MAIR: otherwise even a correct
     * new table can fault while the old regime is being reconfigured.
     *
     * The kernel and BootInfo are identity-mapped by the firmware/loader map,
     * so execution and data accesses remain physical while M is clear.
     * When called from EL2, these writes prepare the inactive EL1 regime.
     */
    movz    x9, #0x0800
    movk    x9, #0x3050, lsl #16   /* SCTLR_EL1_RES1, EE=0, E0E=0, M/C/I=0 */
    msr     sctlr_el1, x9
    dsb     sy
    isb

    msr     ttbr0_el1, x21
    msr     ttbr1_el1, x22
    msr     tcr_el1, x23
    msr     mair_el1, x24
    dsb     sy

    /* Remove stale firmware translations before enabling the new regime. */
    tlbi    vmalle1
    dsb     ish
    isb

    /* Enable the new translation roots from a known EL1 state. This is the
     * minimal Linux-style AArch64 baseline: RES1 plus M/C/I, little-endian,
     * with optional controls left disabled until the kernel configures them. */
    movz    x9, #0x1805
    movk    x9, #0x30d0, lsl #16   /* SCTLR_EL1_RES1 | M | C | I */
    msr     sctlr_el1, x9
    isb
    ret

el1_install_enter:
    /* UEFI may enter directly at EL1. Install vectors and FP access before
     * touching the MMU so an early synchronous exception has a defined path. */
    adrp    x9, exception_vectors
    add     x9, x9, :lo12:exception_vectors
    msr     vbar_el1, x9
    mov     x9, #(3 << 20)
    msr     cpacr_el1, x9
    isb
    bl      el1_install
    b       el1_entry

el1_entry:
    /* Reassert the kernel's FP/SIMD policy at EL1. Other optional coprocessor
     * features remain disabled until their own architectural setup exists. */
    mov     x2, #(3 << 20)
    msr     cpacr_el1, x2
    isb

    /* Move to the higher half. The stack and the continuation are physical
     * addresses biased by `kernel_va_start - kernel_phys_start`. */
    adrp    x9, __stack_top
    add     x9, x9, :lo12:__stack_top
    add     x9, x9, x26
    mov     sp, x9

    adrp    x9, el1_high
    add     x9, x9, :lo12:el1_high
    add     x9, x9, x26
    br      x9

el1_high:
    /*
     * First visible kernel-side checkpoint. The loader mapped the GOP buffer
     * at PHYSMAP_BASE + physical_address in TTBR1. Paint a 16x16 green marker
     * in the upper-left corner before entering Rust; this proves that _start
     * survived the EL transition, installed the new translation regime and
     * reached the higher-half continuation.
     *
     * BootInfo.framebuffer.address/stride are at offsets 104/120.
     */
    ldr     x9, [x20, #104]
    cbz     x9, 5f
    movz    x10, #0x0000
    movk    x10, #0xc000, lsl #32
    movk    x10, #0xffff, lsl #48
    add     x9, x9, x10
    ldr     w11, [x20, #120]
    mov     w12, #0x0000ff00
    mov     x13, #0
5:
    cbz     x9, 8f
6:
    cmp     x13, #16
    b.hs    8f
    mov     x14, #0
7:
    cmp     x14, #16
    b.hs    9f
    madd    x15, x13, x11, x14
    str     w12, [x9, x15, lsl #2]
    add     x14, x14, #1
    b       7b
9:
    add     x13, x13, #1
    b       6b
8:
    dsb     sy

    /* Install the kernel vector table now that its virtual address is live.
     * PC-relative references already produce higher-half addresses here, so
     * the bias must not be added a second time. */
    adrp    x2, exception_vectors
    add     x2, x2, :lo12:exception_vectors
    msr     vbar_el1, x2
    isb

    /* x0 remains the physical BootInfo address; x1 reports the exception level
     * firmware used before the EL2 drop. */
    mov     x0, x20
    mov     x1, x19
    bl      kmain

    /* kmain is noreturn; if it ever returns, park the core. */
park:
    wfe
    b       park

    .size _start, . - _start

/*
 * Raw PSCI call.
 *
 *   x0 = function identifier
 *   x1 = argument 1
 *   x2 = argument 2
 *   x3 = argument 3
 *   x4 = 0 for SMC, 1 for HVC
 *
 * The result is returned in x0. The link register is preserved across the
 * firmware call so a normal PSCI query or CPU_ON can return to Rust. PSCI
 * SYSTEM_OFF never returns; if firmware rejects it the caller parks.
 */
    .section ".text.psci_call", "ax"
    .global psci_call_asm
    .type psci_call_asm, %function

psci_call_asm:
    /* Keep LR in a callee-saved register. Firmware may clobber x0..x18. */
    stp     x19, x20, [sp, #-16]!
    mov     x19, x30
    cbz     x4, 1f
    hvc     #0
    b       2f
1:
    smc     #0
2:
    mov     x30, x19
    ldp     x19, x20, [sp], #16
    ret

    .size psci_call_asm, . - psci_call_asm

/*
 * Secondary-core entry installed in PSCI CPU_ON.
 *
 *   x0 = physical address of this core's PerCpu record (PSCI context)
 *
 * PSCI starts the core with the MMU and caches off, at the highest non-secure
 * exception level it implements. On a machine without EL2 that is EL1; with
 * virtualization enabled it can be EL2. Both paths are handled here.
 *
 * The record offsets are the ABI documented in `percpu.rs`:
 *   0  stack_top (physical)   8  ttbr0   16 tcr   24 mair
 *   32 sctlr                  40 cpacr   48 vbar  56 index
 *   64 mpidr                  72 Rust entry (high)  80 ttbr1  88 bias
 *
 * The boot core has already cleaned the record, the page tables, the stacks
 * and this code to the point of coherency. This stub must not touch memory
 * outside the record before it installs the stack.
 */
    .section ".text.secondary_entry", "ax"
    .global secondary_entry
    .type secondary_entry, %function

secondary_entry:
    msr     daifset, #0xf
    mov     x19, x0

    mrs     x9, CurrentEL
    lsr     x9, x9, #2
    and     x9, x9, #3
    cmp     x9, #2
    b.eq    secondary_from_el2
    cmp     x9, #1
    b.ne    secondary_park
    b       secondary_at_el1

secondary_from_el2:
    /* EL1 is AArch64, no VHE, and no traps from EL1 to EL2. Clearing TSC
     * lets PSCI SMC calls from EL1 reach EL3 directly. */
    mov     x9, #(1 << 31)          /* HCR_RW */
    msr     hcr_el2, x9
    isb

    /* Allow EL1 to read the physical counter and program the physical timer. */
    mov     x9, #3                  /* EL1PCTEN | EL1PCEN */
    msr     cnthctl_el2, x9
    msr     cntvoff_el2, xzr
    isb

    /* Do not trap FP/SIMD, SVE or other EL1 accesses to EL2. */
    mrs     x9, cptr_el2
    mov     x10, #(1 << 8)          /* TZ */
    bic     x9, x9, x10
    mov     x10, #(1 << 10)         /* TFP */
    bic     x9, x9, x10
    mov     x10, #(1 << 12)         /* TCPAC */
    bic     x9, x9, x10
    mov     x10, #(1 << 20)         /* TTA */
    bic     x9, x9, x10
    msr     cptr_el2, x9
    isb

    /* Let EL1 use the system-register GIC interface when it exists. */
    mrs     x9, id_aa64pfr0_el1
    ubfx    x9, x9, #24, #4
    cbz     x9, 1f
    mov     x9, #0xf                /* SRE | DFB | DIB | Enable */
    msr     ICC_SRE_EL2, x9
    isb
1:
    bl      secondary_install_state
    adrp    x9, secondary_el1_enable
    add     x9, x9, :lo12:secondary_el1_enable
    msr     elr_el2, x9
    mov     x9, #0x3c5              /* DAIF masked, EL1h, AArch64 */
    msr     spsr_el2, x9
    ldr     x9, [x19, #0]
    msr     sp_el1, x9
    isb
    eret

secondary_at_el1:
    bl      secondary_install_state
    ldr     x9, [x19, #0]
    mov     sp, x9
    b       secondary_el1_enable

/* Install the EL1 translation state and per-core pointer. Safe at EL1 or EL2
 * because EL2 may write the EL1 registers while HCR_EL2 does not trap them.
 * The record itself is still addressed physically here; TPIDR_EL1 is set to
 * its higher-half alias because every later access happens after the switch. */
secondary_install_state:
    ldr     x9, [x19, #8]
    msr     ttbr0_el1, x9
    ldr     x9, [x19, #80]
    msr     ttbr1_el1, x9
    ldr     x9, [x19, #16]
    msr     tcr_el1, x9
    ldr     x9, [x19, #24]
    msr     mair_el1, x9
    ldr     x9, [x19, #40]
    msr     cpacr_el1, x9
    ldr     x9, [x19, #48]
    msr     vbar_el1, x9
    ldr     x9, [x19, #88]
    add     x9, x19, x9
    msr     tpidr_el1, x9
    isb
    ret

secondary_el1_enable:
    /* Discard any firmware TLB entries before the new tables take effect. */
    tlbi    vmalle1
    dsb     ish
    isb

    /* SCTLR_EL1 already has M, C and I set by the boot core. */
    ldr     x9, [x19, #32]
    msr     sctlr_el1, x9
    isb

    /* Move to the higher-half stack and enter Rust at its virtual address.
     * x0 becomes the higher-half alias of the PerCpu record. */
    ldr     x10, [x19, #88]
    add     x0, x19, x10
    ldr     x9, [x19, #0]
    add     x9, x9, x10
    mov     sp, x9
    ldr     x9, [x19, #72]
    br      x9

secondary_park:
    wfe
    b       secondary_park

    .size secondary_entry, . - secondary_entry
