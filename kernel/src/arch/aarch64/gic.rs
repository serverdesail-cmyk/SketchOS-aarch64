//! GICv3/GICv4 distributor, redistributor and CPU-interface bring-up.
//!
//! The distributor is initialized once by the boot core. Every core, including
//! the boot core, then initializes its own redistributor and system-register
//! CPU interface. This is the split the architecture requires: `GICD_*` is
//! global, `GICR_*` and `ICC_*_EL1` are private to a core.
//!
//! The code uses the architecture's system-register CPU interface
//! (`ICC_*_EL1`), which is the interface GICv3 and GICv4 expose to EL1. It
//! does not guess GICv2 register layouts and it does not touch an ITS.
//!
//! Every register offset below is from the Arm Generic Interrupt Controller
//! Architecture Specification, GICv3/GICv4. The addresses themselves come from
//! MADT, never from a hard-coded board guess.
//!
//! The distributor is enabled for Group 1 non-secure interrupts only. A
//! non-secure OS must not clear secure enable bits, so `GICD_CTLR` is modified
//! read-modify-write with the non-secure bits rather than overwritten with a
//! constant.
//!
//! # SGIs
//!
//! Software-generated interrupts are private interrupts (INTID 0..15). SGIs 0
//! and 1 are reserved for the kernel's ping/pong IPI test, 2 asks a core to
//! reschedule, and 3 asks a core to push a runnable entity to an idle core.
//! `ICC_SGI1R_EL1` targets a set of cores by affinity, using the hardware
//! MPIDR value from MADT. No logical CPU index is ever written to the
//! controller.

#![allow(unsafe_op_in_unsafe_fn)]

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::acpi::{Platform, MPIDR_AFFINITY_MASK};
use crate::arch::aarch64::{current_el, mpidr};
use crate::println;
use sketchos_bootinfo::phys_to_virt;

const GICD_CTLR: u64 = 0x0000;

// GICD_CTLR as seen from a non-secure access when the GIC implements two
// security states and DS == 0. The register is renumbered: bit [4] is
// ARE_NS (an alias of secure ARE_S) and bit [1] is EnableGrp1A, the group
// enable that maps to secure EnableGrp1NS. Secure-only bits such as
// EnableGrp0 are RES0 here, so they must not be written.
const GICD_CTLR_ENABLE_GRP1: u32 = 1 << 1;
const GICD_CTLR_ARE_NS: u32 = 1 << 4;
const GICD_CTLR_RWP: u32 = 1 << 31;

const GICR_TYPER: u64 = 0x0008;
const GICR_WAKER: u64 = 0x0014;
const GICR_WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
const GICR_WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
const GICR_TYPER_VLPIS: u64 = 1 << 1;
const GICR_TYPER_LAST: u64 = 1 << 4;
const GICR_FRAME_SIZE: u64 = 0x20000;
const GICR_FRAME_SIZE_V4: u64 = 0x40000;
const GICR_SGI_OFFSET: u64 = 0x10000;

const GICR_IGROUPR0: u64 = 0x0080;
const GICR_ISENABLER0: u64 = 0x0100;
const GICR_IPRIORITYR0: u64 = 0x0400;
const GICR_ICFGR1: u64 = 0x0c04;
const GICR_IGRPMODR0: u64 = 0x0d00;

/// Architectural EL1 virtual timer PPI (INTID 27). The virtual timer is the
/// one available to a guest under a hypervisor (WHPX exposes only CNTV_*),
/// and it is self-consistent on bare metal because CNTVCT_EL0 and
/// CNTV_CVAL_EL0 share the same CNTVOFF_EL2 offset.
const TIMER_PPI_DEFAULT: u32 = 27;

/// SGI used by the boot core to ask a secondary to answer.
pub const SGI_PING: u32 = 0;
/// SGI used by a secondary to answer the boot core.
pub const SGI_PONG: u32 = 1;
/// Ask a core to re-evaluate its run queue immediately.
pub const SGI_RESCHED: u32 = 2;
/// Ask a core to push work to an idle core.
pub const SGI_BALANCE: u32 = 3;

/// Normal priority for the timer and the two SGIs. `ICC_PMR_EL1` is 0xff, so
/// anything below that is accepted.
const PRIVATE_PRIORITY: u8 = 0xa0;

static GICD_BASE: AtomicU64 = AtomicU64::new(0);
static GICR_BASE: AtomicU64 = AtomicU64::new(0);
static TIMER_IRQ: AtomicU32 = AtomicU32::new(TIMER_PPI_DEFAULT);
static READY: AtomicBool = AtomicBool::new(false);

/// Initialize the distributor, the boot CPU's redistributor and the boot CPU's
/// CPU interface. The timer PPI is enabled but the CPU's IRQ line is not
/// unmasked; the scheduler's first exception return does that.
pub fn init(platform: &Platform) -> Result<(), &'static str> {
    if current_el() != 1 {
        return Err("GICv3 CPU interface requires EL1");
    }

    // SAFETY: the distributor base comes from MADT and is mapped Device.
    let gicd = unsafe { init_distributor(platform)? };
    let gicr = init_cpu(platform, mpidr())?;

    GICD_BASE.store(gicd, Ordering::Release);
    GICR_BASE.store(gicr, Ordering::Release);
    READY.store(true, Ordering::Release);
    Ok(())
}

/// Initialize one core's redistributor and CPU interface.
///
/// Safe to call once per core. The distributor must already be enabled by
/// [`init`] on the boot core. Returns the redistributor frame base so the
/// caller can record it for diagnostics.
pub fn init_cpu(platform: &Platform, core_mpidr: u64) -> Result<u64, &'static str> {
    if current_el() != 1 {
        return Err("GICv3 CPU interface requires EL1");
    }

    let timer_irq = timer_irq_from_platform(platform);
    if timer_irq >= 32 {
        return Err("GTDT timer interrupt is not a private interrupt");
    }

    // SAFETY: every MMIO address comes from MADT and is mapped Device-nGnRnE
    // by the kernel page tables before this function is called.
    unsafe {
        let gicr = find_redistributor(platform, core_mpidr)?;
        wake_redistributor(gicr)?;
        configure_private_interrupts(gicr, timer_irq);
        enable_cpu_interface()?;
        TIMER_IRQ.store(timer_irq, Ordering::Release);
        Ok(gicr)
    }
}

/// True after a successful [`init`] on the boot core.
pub fn is_ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// Timer PPI number selected from GTDT, or the architectural default.
pub fn timer_irq() -> u32 {
    TIMER_IRQ.load(Ordering::Acquire)
}

/// Distributor and redistributor bases, for diagnostics.
pub fn bases() -> (u64, u64) {
    (
        GICD_BASE.load(Ordering::Acquire),
        GICR_BASE.load(Ordering::Acquire),
    )
}

/// Acknowledge the highest-priority Group 1 interrupt.
pub fn acknowledge() -> u32 {
    let value: u64;
    // SAFETY: ICC_IAR1_EL1 is readable at EL1 after SRE is enabled. Reading it
    // acknowledges the interrupt, so this is only called from the IRQ handler.
    unsafe {
        asm!("mrs {}, ICC_IAR1_EL1", out(reg) value, options(nomem, nostack));
    }
    (value & 0x00ff_ffff) as u32
}

/// Signal end-of-interrupt for a Group 1 interrupt.
pub fn end(intid: u32) {
    // SAFETY: only called with an INTID returned by `acknowledge`.
    unsafe {
        asm!(
            "msr ICC_EOIR1_EL1, {}",
            in(reg) intid as u64,
            options(nomem, nostack)
        );
        asm!("dsb sy", options(nomem, nostack));
    }
}

/// Send an SGI to one core identified by its hardware MPIDR affinity value.
///
/// `target_mpidr` must be the value from MADT/PSCI, not a logical index. Aff0
/// must fit the 16-bit target list; the architecture has no way to address
/// Aff0 values above 15 with a single SGI write.
pub fn send_sgi(target_mpidr: u64, intid: u32) -> Result<(), &'static str> {
    if intid > 15 {
        return Err("SGI INTID must be in 0..=15");
    }
    let aff0 = target_mpidr & 0xff;
    if aff0 >= 16 {
        return Err("target Aff0 is outside the 16-bit SGI target list");
    }
    let aff1 = (target_mpidr >> 8) & 0xff;
    let aff2 = (target_mpidr >> 16) & 0xff;
    let aff3 = (target_mpidr >> 32) & 0xff;

    // ICC_SGI1R_EL1: TargetList [15:0], Aff1 [23:16], INTID [27:24],
    // Aff2 [39:32], Aff3 [55:48]. IRM [47:40] stays zero so exactly one
    // affinity is addressed.
    let value = (1u64 << aff0)
        | (aff1 << 16)
        | ((intid as u64) << 24)
        | (aff2 << 32)
        | (aff3 << 48);

    // SAFETY: ICC_SGI1R_EL1 is writable at EL1 after the CPU interface is
    // enabled. The target came from firmware tables, and the write only
    // requests an SGI.
    unsafe {
        asm!("msr ICC_SGI1R_EL1, {}", in(reg) value, options(nomem, nostack));
        asm!("dsb sy", "isb", options(nomem, nostack));
    }
    Ok(())
}

/// Unmask IRQs on the current CPU.
///
/// Kept as part of the interrupt-controller API; the scheduler relies on the
/// first `eret` to unmask IRQs instead of calling this directly.
#[allow(dead_code)]
pub fn enable_irq() {
    // SAFETY: clearing DAIF.I only changes the current CPU's interrupt mask.
    unsafe {
        asm!("msr daifclr, #2", "isb", options(nomem, nostack));
    }
}

/// Mask IRQs on the current CPU.
#[allow(dead_code)]
pub fn disable_irq() {
    // SAFETY: setting DAIF.I only changes the current CPU's interrupt mask.
    unsafe {
        asm!("msr daifset, #2", "isb", options(nomem, nostack));
    }
}

/// Print the negotiated controller information.
pub fn report() {
    let (gicd, gicr) = bases();
    println!(
        "GIC: GICv3+ distributor 0x{:016x}, redistributor 0x{:016x}, timer PPI {}",
        gicd,
        gicr,
        timer_irq()
    );
}

fn timer_irq_from_platform(platform: &Platform) -> u32 {
    match platform.timer {
        Some(timer)
            if timer.virtual_el1_gsiv != 0 && timer.virtual_el1_gsiv != u32::MAX =>
        {
            timer.virtual_el1_gsiv
        }
        Some(timer)
            if timer.non_secure_el1_gsiv != 0 && timer.non_secure_el1_gsiv != u32::MAX =>
        {
            timer.non_secure_el1_gsiv
        }
        _ => TIMER_PPI_DEFAULT,
    }
}

/// Pack MPIDR_EL1 Aff0..Aff3 into the 32-bit form GICR_TYPER reports.
///
/// MPIDR places Aff3 at bits 39:32; GICR_TYPER places it at bits 31:24 of its
/// Affinity_Value field. Comparing the raw registers would work only when
/// Aff3 is zero.
fn packed_affinity(mpidr: u64) -> u64 {
    (mpidr & 0x0000_0000_00ff_ffff) | ((mpidr >> 8) & 0x0000_0000_ff00_0000)
}

unsafe fn init_distributor(platform: &Platform) -> Result<u64, &'static str> {
    let gicd = platform
        .gicd
        .ok_or("MADT does not describe a GIC distributor")?;
    if gicd.base == 0 {
        return Err("GIC distributor base is zero");
    }
    if gicd.version < 3 {
        return Err("GIC version below 3 is not supported");
    }

    // Affinity routing is mandatory in GICv3, and the architecture makes
    // ARE_NS read-as-one/write-ignored once security states are enabled.
    // There is no "does this GIC support affinity routing" bit in
    // GICD_TYPER, so the distributor's version is the only check needed.
    //
    // Disable the non-secure group while the redistributors are configured,
    // then enable it again with affinity routing. A non-secure access only
    // reaches the non-secure bits of the register.
    let mut ctlr = read32(gicd.base, GICD_CTLR) & !GICD_CTLR_RWP;
    ctlr &= !GICD_CTLR_ENABLE_GRP1;
    write32(gicd.base, GICD_CTLR, ctlr);
    wait_rwp(gicd.base)?;
    ctlr |= GICD_CTLR_ARE_NS | GICD_CTLR_ENABLE_GRP1;
    write32(gicd.base, GICD_CTLR, ctlr);
    wait_rwp(gicd.base)?;
    Ok(gicd.base)
}

unsafe fn wake_redistributor(gicr: u64) -> Result<(), &'static str> {
    // Wake the redistributor and wait for its children to leave the sleep
    // state. A timeout is a hard failure: programming a sleeping
    // redistributor would silently never deliver the timer or an SGI.
    let waker = read32(gicr, GICR_WAKER);
    write32(gicr, GICR_WAKER, waker & !GICR_WAKER_PROCESSOR_SLEEP);
    let mut spins = 0u32;
    while read32(gicr, GICR_WAKER) & GICR_WAKER_CHILDREN_ASLEEP != 0 {
        spins += 1;
        if spins > 1_000_000 {
            return Err("GIC redistributor did not wake");
        }
    }
    Ok(())
}

unsafe fn configure_private_interrupts(gicr: u64, timer_irq: u32) {
    let sgi = gicr + GICR_SGI_OFFSET;
    let timer_bit = 1u32 << timer_irq;
    let sgi_bits = (1u32 << SGI_PING)
        | (1u32 << SGI_PONG)
        | (1u32 << SGI_RESCHED)
        | (1u32 << SGI_BALANCE);

    // Group 1 non-secure, normal priority. The timer PPI is level-sensitive;
    // SGIs are edge-triggered by definition and GICR_ICFGR0 is read-only for
    // them, so only the timer configuration field is cleared.
    write32(
        sgi,
        GICR_IGROUPR0,
        read32(sgi, GICR_IGROUPR0) | timer_bit | sgi_bits,
    );
    write32(
        sgi,
        GICR_IGRPMODR0,
        read32(sgi, GICR_IGRPMODR0) & !(timer_bit | sgi_bits),
    );
    write8(sgi, GICR_IPRIORITYR0 + timer_irq as u64, PRIVATE_PRIORITY);
    write8(sgi, GICR_IPRIORITYR0 + SGI_PING as u64, PRIVATE_PRIORITY);
    write8(sgi, GICR_IPRIORITYR0 + SGI_PONG as u64, PRIVATE_PRIORITY);
    write8(sgi, GICR_IPRIORITYR0 + SGI_RESCHED as u64, PRIVATE_PRIORITY);
    write8(sgi, GICR_IPRIORITYR0 + SGI_BALANCE as u64, PRIVATE_PRIORITY);

    let config_shift = (timer_irq - 16) * 2;
    let config = read32(sgi, GICR_ICFGR1) & !(0b11 << config_shift);
    write32(sgi, GICR_ICFGR1, config);

    // ISENABLER0 is write-one-to-set; writing the timer and SGI bits enables
    // exactly those private interrupts.
    write32(sgi, GICR_ISENABLER0, timer_bit | sgi_bits);
}

unsafe fn enable_cpu_interface() -> Result<(), &'static str> {
    // Enable the system-register CPU interface. SRE must be set before the
    // ICC_* registers are usable; DIB/DFB make the interface tolerant of
    // firmware that left them clear.
    let mut sre: u64;
    asm!("mrs {}, ICC_SRE_EL1", out(reg) sre, options(nomem, nostack));
    sre |= 0b111;
    asm!("msr ICC_SRE_EL1, {}", in(reg) sre, options(nomem, nostack));
    asm!("isb", options(nomem, nostack));
    let mut check: u64;
    asm!("mrs {}, ICC_SRE_EL1", out(reg) check, options(nomem, nostack));
    if check & 1 == 0 {
        return Err("GIC system-register CPU interface is not available");
    }

    asm!("msr ICC_PMR_EL1, {}", in(reg) 0xffu64, options(nomem, nostack));
    asm!("msr ICC_BPR1_EL1, {}", in(reg) 0u64, options(nomem, nostack));
    asm!("msr ICC_IGRPEN1_EL1, {}", in(reg) 1u64, options(nomem, nostack));
    asm!("dsb sy", "isb", options(nomem, nostack));
    Ok(())
}

/// Find the redistributor frame whose `GICR_TYPER` affinity matches `core_mpidr`.
///
/// The MADT GICR ranges are searched first. If the firmware instead describes
/// a per-CPU GICR base in the GICC entry, that documented address is used as a
/// fallback. Nothing is inferred from the logical CPU index.
unsafe fn find_redistributor(
    platform: &Platform,
    core_mpidr: u64,
) -> Result<u64, &'static str> {
    let affinity = packed_affinity(core_mpidr);

    for range in platform.gicr[..platform.gicr_count].iter() {
        if range.base == 0 {
            continue;
        }

        // GICv4 redistributors have a 256 KiB stride and a VLPI frame. The
        // VLPIS bit in the first frame tells us which stride to use.
        let first_typer = read64(range.base, GICR_TYPER);
        let stride = if first_typer & GICR_TYPER_VLPIS != 0 {
            GICR_FRAME_SIZE_V4
        } else {
            GICR_FRAME_SIZE
        };
        let end = range.base.saturating_add(range.length as u64);
        let mut base = range.base;
        while base.saturating_add(stride) <= end {
            let typer = read64(base, GICR_TYPER);
            if typer != 0 && ((typer >> 32) & 0xffff_ffff) == affinity {
                return Ok(base);
            }
            if typer & GICR_TYPER_LAST != 0 {
                break;
            }
            base = base.saturating_add(stride);
        }
    }

    let needle = core_mpidr & MPIDR_AFFINITY_MASK;
    for cpu in platform.cpus[..platform.cpu_count].iter() {
        if cpu.enabled && (cpu.mpidr & MPIDR_AFFINITY_MASK) == needle && cpu.gicr != 0 {
            return Ok(cpu.gicr);
        }
    }

    Err("no GIC redistributor matches this CPU")
}

unsafe fn wait_rwp(gicd: u64) -> Result<(), &'static str> {
    let mut spins = 0u32;
    while read32(gicd, GICD_CTLR) & GICD_CTLR_RWP != 0 {
        spins += 1;
        if spins > 1_000_000 {
            return Err("GIC distributor RWP timeout");
        }
    }
    Ok(())
}

unsafe fn read32(base: u64, offset: u64) -> u32 {
    core::ptr::read_volatile(phys_to_virt(base + offset) as *const u32)
}

unsafe fn read64(base: u64, offset: u64) -> u64 {
    core::ptr::read_volatile(phys_to_virt(base + offset) as *const u64)
}

unsafe fn write32(base: u64, offset: u64, value: u32) {
    core::ptr::write_volatile(phys_to_virt(base + offset) as *mut u32, value);
}

unsafe fn write8(base: u64, offset: u64, value: u8) {
    core::ptr::write_volatile(phys_to_virt(base + offset) as *mut u8, value);
}
