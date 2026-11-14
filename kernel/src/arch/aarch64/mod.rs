//! AArch64 system-register access, exception vectors and the final halt loop.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::{print, println};

pub mod cache;
pub mod gic;
pub mod mmu;
pub mod psci;
pub mod timer;

// The assembly entry point and exception vectors live beside this module.
// `global_asm!` keeps them in the same translation unit so the linker always
// retains the symbols the kernel depends on.
core::arch::global_asm!(include_str!("boot.S"));
core::arch::global_asm!(include_str!("vectors.S"));

extern "C" {
    /// 2 KiB-aligned AArch64 exception vector table.
    fn exception_vectors();
    /// Enter a task from a scheduler-built trap frame in `x0`.
    fn scheduler_enter(frame: *mut TrapFrame) -> !;
}

/// Register and fault state captured by `vectors.S`.
///
/// The layout is part of the assembly contract. `size_of::<TrapFrame>()` must
/// stay 816 bytes, and `q` must start at offset 288. `sp_el0` carries the user
/// stack pointer across an EL0 entry or a context switch.
#[repr(C, align(16))]
pub struct TrapFrame {
    pub x: [u64; 31],
    pub elr: u64,
    pub spsr: u64,
    pub esr: u64,
    pub far: u64,
    pub sp_el0: u64,
    pub q: [u128; 32],
    pub fpsr: u64,
    pub fpcr: u64,
}

const _: () = assert!(core::mem::size_of::<TrapFrame>() == 816);
const _: () = assert!(core::mem::offset_of!(TrapFrame, sp_el0) == 280);
const _: () = assert!(core::mem::offset_of!(TrapFrame, q) == 288);
const _: () = assert!(core::mem::offset_of!(TrapFrame, fpsr) == 800);
const _: () = assert!(core::mem::offset_of!(TrapFrame, fpcr) == 808);

/// Install the exception vector table and return its base address.
///
/// VBAR_EL1 is always programmed because the kernel runs at EL1 after
/// `boot.S` drops from EL2 when needed. VBAR_EL2 and VBAR_EL3 are programmed
/// only when the current exception level permits access.
pub fn install_exception_vectors() -> u64 {
    let base = exception_vectors as *const () as usize as u64;
    let el = current_el();

    // SAFETY: VBAR_ELx is writable at the corresponding exception level or
    // higher, and `base` points at the 2 KiB-aligned table assembled above.
    unsafe {
        asm!("msr vbar_el1, {}", in(reg) base, options(nostack));
        if el >= 2 {
            asm!("msr vbar_el2, {}", in(reg) base, options(nostack));
        }
        if el >= 3 {
            asm!("msr vbar_el3, {}", in(reg) base, options(nostack));
        }
        asm!("isb", options(nostack));
    }

    base
}

/// Enter a task from a scheduler-built trap frame.
pub fn enter_task(frame: *mut TrapFrame) -> ! {
    // SAFETY: the caller built the frame at the top of a live kernel stack and
    // left IRQs masked; `scheduler_enter` installs that frame and `eret`s.
    unsafe { scheduler_enter(frame) }
}

/// Description of a synchronous fault, passed to the user-fault hook.
///
/// 0.0.4 routes kernel faults through the probe path only; the 0.0.5 process
/// layer is the first consumer of the remaining fields.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub struct FaultInfo {
    /// Vector index: 0/4 is a kernel fault, 8..=15 a lower-EL fault.
    pub vector: u64,
    pub elr: u64,
    pub spsr: u64,
    pub esr: u64,
    pub far: u64,
    /// True when the fault came from EL0 (or AArch32 user mode).
    pub from_user: bool,
}

/// Decision a [`UserFaultHook`] returns for a lower-EL fault.
#[allow(dead_code)] // Constructed by the 0.0.5 user-fault hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultAction {
    /// Return to the faulting instruction unchanged (it will fault again
    /// unless the hook changed the mapping or the register state).
    Retry,
    /// Skip the faulting instruction and resume at `ELR + 4`.
    Skip,
    /// The fault is not recoverable for this thread.
    Terminate,
}

/// Callback invoked for a synchronous fault from a lower exception level.
///
/// 0.0.5 installs [`crate::process::user_fault`], which terminates the faulting
/// process. A kernel fault never reaches the hook.
pub type UserFaultHook = fn(&FaultInfo) -> FaultAction;

static USER_FAULT_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Install (or clear) the lower-EL synchronous-fault hook.
///
/// # Safety
///
/// The function must be safe to call from exception context: it runs with IRQs
/// masked, on the faulting core's kernel stack, and must not block.
#[allow(dead_code)] // Kept for bring-up probing and the 0.0.5 loader.
pub unsafe fn set_user_fault_hook(hook: Option<UserFaultHook>) {
    let address = match hook {
        Some(hook) => hook as usize,
        None => 0,
    };
    USER_FAULT_HOOK.store(address, Ordering::Release);
}

/// Global early-boot probe state. Probes are single-core and non-reentrant;
/// callers must serialize them and keep IRQs masked until a per-core probe
/// context replaces this bootstrap mechanism.
static PROBE_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Address the exception handler resumes a faulting probe at.
static PROBE_RESUME: AtomicU64 = AtomicU64::new(0);

/// Read a `u64` from a possibly unmapped address without taking a fatal fault.
///
/// The kernel normally treats a synchronous fault as fatal. A probe marks a
/// resume point first; if the load aborts, the exception handler rewrites
/// `ELR` to that point and the probe reports `Err`.
#[allow(dead_code)] // Exercised by the 0.0.5 loader, kept for bring-up probing.
pub fn probe_read(address: u64) -> Result<u64, ()> {
    let value: u64;
    let status: u64;
    // SAFETY: the sequence writes only its own resume bookkeeping, and a fault
    // is redirected by `exception_handler` to the local `2:` label.
    unsafe {
        asm!(
            "adr x9, 2f",
            "str x9, [{resume}]",
            "mov w10, #1",
            "strb w10, [{active}]",
            "1:",
            "ldr {value}, [{address}]",
            "mov {status}, #1",
            "b 3f",
            "2:",
            "mov {status}, #0",
            "3:",
            "strb wzr, [{active}]",
            resume = in(reg) core::ptr::addr_of!(PROBE_RESUME),
            active = in(reg) core::ptr::addr_of!(PROBE_ACTIVE),
            address = in(reg) address,
            value = out(reg) value,
            status = out(reg) status,
            out("x9") _,
            out("x10") _,
            options(nostack),
        );
    }
    if status != 0 {
        Ok(value)
    } else {
        Err(())
    }
}

/// Read an 8-bit MMIO/configuration value without taking a fatal abort.
pub fn probe_read8(address: u64) -> Result<u8, ()> {
    let value: u32;
    let status: u64;
    // SAFETY: the exception handler redirects a synchronous abort to label 2.
    unsafe {
        asm!(
            "adr x9, 2f",
            "str x9, [{resume}]",
            "mov w10, #1",
            "strb w10, [{active}]",
            "1:",
            "ldrb {value:w}, [{address}]",
            "mov {status}, #1",
            "b 3f",
            "2:",
            "mov {status}, #0",
            "3:",
            "strb wzr, [{active}]",
            resume = in(reg) core::ptr::addr_of!(PROBE_RESUME),
            active = in(reg) core::ptr::addr_of!(PROBE_ACTIVE),
            address = in(reg) address,
            value = out(reg) value,
            status = out(reg) status,
            out("x9") _,
            out("x10") _,
            options(nostack),
        );
    }
    if status != 0 { Ok(value as u8) } else { Err(()) }
}

/// Read a 16-bit MMIO/configuration value without taking a fatal abort.
pub fn probe_read16(address: u64) -> Result<u16, ()> {
    let value: u32;
    let status: u64;
    // SAFETY: the exception handler redirects a synchronous abort to label 2.
    unsafe {
        asm!(
            "adr x9, 2f",
            "str x9, [{resume}]",
            "mov w10, #1",
            "strb w10, [{active}]",
            "1:",
            "ldrh {value:w}, [{address}]",
            "mov {status}, #1",
            "b 3f",
            "2:",
            "mov {status}, #0",
            "3:",
            "strb wzr, [{active}]",
            resume = in(reg) core::ptr::addr_of!(PROBE_RESUME),
            active = in(reg) core::ptr::addr_of!(PROBE_ACTIVE),
            address = in(reg) address,
            value = out(reg) value,
            status = out(reg) status,
            out("x9") _,
            out("x10") _,
            options(nostack),
        );
    }
    if status != 0 { Ok(value as u16) } else { Err(()) }
}

/// Read a 32-bit MMIO/configuration value without taking a fatal abort.
pub fn probe_read32(address: u64) -> Result<u32, ()> {
    let value: u32;
    let status: u64;
    // SAFETY: the exception handler redirects a synchronous abort to label 2.
    unsafe {
        asm!(
            "adr x9, 2f",
            "str x9, [{resume}]",
            "mov w10, #1",
            "strb w10, [{active}]",
            "1:",
            "ldr {value:w}, [{address}]",
            "mov {status}, #1",
            "b 3f",
            "2:",
            "mov {status}, #0",
            "3:",
            "strb wzr, [{active}]",
            resume = in(reg) core::ptr::addr_of!(PROBE_RESUME),
            active = in(reg) core::ptr::addr_of!(PROBE_ACTIVE),
            address = in(reg) address,
            value = out(reg) value,
            status = out(reg) status,
            out("x9") _,
            out("x10") _,
            options(nostack),
        );
    }
    if status != 0 { Ok(value) } else { Err(()) }
}

/// Write a `u64` to a possibly unwritable address without taking a fatal fault.
#[allow(dead_code)] // Kept for bring-up probing; the 0.0.5 loader may use it.
pub fn probe_write(address: u64, value: u64) -> Result<(), ()> {
    let status: u64;
    // SAFETY: see `probe_read`.
    unsafe {
        asm!(
            "adr x9, 2f",
            "str x9, [{resume}]",
            "mov w10, #1",
            "strb w10, [{active}]",
            "1:",
            "str {value}, [{address}]",
            "mov {status}, #1",
            "b 3f",
            "2:",
            "mov {status}, #0",
            "3:",
            "strb wzr, [{active}]",
            resume = in(reg) core::ptr::addr_of!(PROBE_RESUME),
            active = in(reg) core::ptr::addr_of!(PROBE_ACTIVE),
            address = in(reg) address,
            value = in(reg) value,
            status = out(reg) status,
            out("x9") _,
            out("x10") _,
            options(nostack),
        );
    }
    if status != 0 {
        Ok(())
    } else {
        Err(())
    }
}

/// Branch to an address that may be unmapped.
///
/// The target must be a leaf function that returns to `LR`; if the branch
/// itself faults, the probe reports `Err`. The target's clobbers are not
/// visible to the compiler, so this is only sound for a target that preserves
/// the AArch64 callee-saved registers.
#[allow(dead_code)] // Kept for bring-up probing; the 0.0.5 loader may use it.
pub fn probe_execute(address: u64) -> Result<(), ()> {
    let status: u64;
    // SAFETY: see `probe_read`. x30 is saved by the sequence so a normally
    // returning leaf cannot corrupt the caller's link register.
    unsafe {
        asm!(
            "adr x9, 3f",
            "str x9, [{resume}]",
            "mov w10, #1",
            "strb w10, [{active}]",
            "mov x11, x30",
            "1:",
            "blr {address}",
            "mov x30, x11",
            "mov {status}, #1",
            "b 4f",
            // A fault may have already clobbered x30 in the `blr`, so both
            // paths restore the caller's link register from x11.
            "3:",
            "mov x30, x11",
            "mov {status}, #0",
            "4:",
            "strb wzr, [{active}]",
            resume = in(reg) core::ptr::addr_of!(PROBE_RESUME),
            active = in(reg) core::ptr::addr_of!(PROBE_ACTIVE),
            address = in(reg) address,
            status = out(reg) status,
            out("x9") _,
            out("x10") _,
            out("x11") _,
            options(nostack),
        );
    }
    if status != 0 {
        Ok(())
    } else {
        Err(())
    }
}

/// Synchronous/FIQ/SError entry. Returns the frame to restore.
///
/// A synchronous exception is recoverable in exactly two cases:
///
/// * a kernel probe marked [`PROBE_ACTIVE`] faults; `ELR` is redirected to the
///   probe's resume label and the kernel continues, or
/// * the fault came from a lower EL and a [`UserFaultHook`] decides how to
///   continue.
///
/// Everything else is reported and halts the core. IRQs never reach this path.
#[no_mangle]
pub extern "C" fn exception_handler(index: u64, frame: *mut TrapFrame) -> *mut TrapFrame {
    // SAFETY: vectors.S passes the 816-byte frame it built on this core's
    // kernel stack and transfers ownership of it to this handler.
    let frame = unsafe { &mut *frame };
    let class = (frame.esr >> 26) & 0x3f;
    let from_user = index >= 8;
    let synchronous = index == 0 || index == 4 || index == 8 || index == 12;

    if synchronous && !from_user && is_abort(class) && PROBE_ACTIVE.load(Ordering::Acquire) {
        let resume = PROBE_RESUME.load(Ordering::Acquire);
        if resume != 0 {
            frame.elr = resume;
            return frame;
        }
    }

    if synchronous && from_user {
        // SVC from EL0 is the system-call entry, not a fault. The dispatcher
        // returns the frame to resume: the same one for a normal return, or a
        // different thread's frame after a blocking call, yield or exit.
        if class == 0x15 {
            return crate::syscall::dispatch(frame as *mut TrapFrame);
        }

        let address = USER_FAULT_HOOK.load(Ordering::Acquire);
        if address != 0 {
            // SAFETY: the stored value came from a `UserFaultHook`.
            let hook: UserFaultHook = unsafe { core::mem::transmute(address) };
            let info = FaultInfo {
                vector: index,
                elr: frame.elr,
                spsr: frame.spsr,
                esr: frame.esr,
                far: frame.far,
                from_user,
            };
            match hook(&info) {
                FaultAction::Retry => return frame,
                FaultAction::Skip => {
                    frame.elr = frame.elr.wrapping_add(4);
                    return frame;
                }
                FaultAction::Terminate => {
                    // The 0.0.5 process layer reaps the faulting process and
                    // returns another thread's frame, or a frame that finishes
                    // the user phase when nothing is runnable.
                    return crate::process::terminate_current(frame as *mut TrapFrame);
                }
            }
        }
    }

    report_exception(index, frame);
    if synchronous && !from_user {
        println!("Halted after a kernel exception");
    } else {
        println!("Halted: no user-fault hook recovered this exception");
    }
    halt()
}

fn is_abort(class: u64) -> bool {
    matches!(class, 0x20..=0x25)
}

fn report_exception(index: u64, frame: &TrapFrame) {
    println!();
    println!("EXCEPTION: vector {} ({})", index, vector_name(index));
    println!(
        "  ELR 0x{:016x}  SPSR 0x{:016x}  ESR 0x{:016x}  FAR 0x{:016x}",
        frame.elr, frame.spsr, frame.esr, frame.far
    );
    println!(
        "  exception class 0x{:02x} ({}), instruction specific 0x{:06x}",
        (frame.esr >> 26) & 0x3f,
        abort_name((frame.esr >> 26) & 0x3f),
        (frame.esr >> 26) & 0x00ff_ffff
    );

    for (row, chunk) in frame.x.chunks(4).enumerate() {
        print!("  ");
        for (column, value) in chunk.iter().enumerate() {
            let register = row * 4 + column;
            print!("x{:02} 0x{:016x}  ", register, value);
        }
        println!();
    }
}

fn abort_name(class: u64) -> &'static str {
    match class {
        0x00 => "unknown",
        0x01 => "trapped WF*",
        0x0e => "illegal execution state",
        0x15 => "SVC",
        0x18 => "trapped MSR/MRS",
        0x20 => "instruction abort from lower EL",
        0x21 => "instruction abort from same EL",
        0x22 => "PC alignment",
        0x24 => "data abort from lower EL",
        0x25 => "data abort from same EL",
        0x26 => "SP alignment",
        0x2f => "SError",
        0x3c => "BRK",
        _ => "other",
    }
}

fn vector_name(index: u64) -> &'static str {
    match index {
        0 => "current EL SP0, synchronous",
        1 => "current EL SP0, IRQ",
        2 => "current EL SP0, FIQ",
        3 => "current EL SP0, SError",
        4 => "current EL SPx, synchronous",
        5 => "current EL SPx, IRQ",
        6 => "current EL SPx, FIQ",
        7 => "current EL SPx, SError",
        8 => "lower EL AArch64, synchronous",
        9 => "lower EL AArch64, IRQ",
        10 => "lower EL AArch64, FIQ",
        11 => "lower EL AArch64, SError",
        12 => "lower EL AArch32, synchronous",
        13 => "lower EL AArch32, IRQ",
        14 => "lower EL AArch32, FIQ",
        15 => "lower EL AArch32, SError",
        _ => "unknown",
    }
}

/// Current exception level (0..=3).
pub fn current_el() -> u64 {
    let value: u64;
    // SAFETY: reading CurrentEL is always allowed and has no side effects.
    unsafe { asm!("mrs {}, CurrentEL", out(reg) value, options(nomem, nostack)) };
    (value >> 2) & 0x3
}

/// Affinity identifier of the executing core.
pub fn mpidr() -> u64 {
    let value: u64;
    // SAFETY: MPIDR_EL1 is readable at EL1 and EL2.
    unsafe { asm!("mrs {}, MPIDR_EL1", out(reg) value, options(nomem, nostack)) };
    value
}

/// Counter timer frequency in Hz.
pub fn counter_frequency() -> u64 {
    let value: u64;
    // SAFETY: CNTFRQ_EL0 is readable at all exception levels.
    unsafe { asm!("mrs {}, CNTFRQ_EL0", out(reg) value, options(nomem, nostack)) };
    value
}

/// Virtual counter value. Divides by [`counter_frequency`] to give seconds. The
/// virtual counter is used because it is accessible at EL1 and is the counter
/// firmware configures for the OS.
pub fn counter() -> u64 {
    let value: u64;
    // SAFETY: CNTVCT_EL0 is readable at EL1 and EL0.
    unsafe { asm!("mrs {}, CNTVCT_EL0", out(reg) value, options(nomem, nostack)) };
    value
}

/// Read `DAIF` and mask IRQs, returning the previous value for
/// [`irq_restore`].
pub fn irq_save_disable() -> u64 {
    let flags: u64;
    // SAFETY: DAIF is readable and writable at EL1. The compiler must not move
    // memory accesses across this sequence, so the `nomem` option is omitted.
    unsafe {
        asm!("mrs {}, DAIF", out(reg) flags);
        asm!("msr DAIFSet, #2");
    }
    flags
}

/// Restore a `DAIF` value saved by [`irq_save_disable`].
pub fn irq_restore(flags: u64) {
    // SAFETY: DAIF is writable at EL1.
    unsafe { asm!("msr DAIF, {}", in(reg) flags) };
}

/// Read `TPIDR_EL1`, the per-core data pointer.
pub fn tpidr_el1() -> u64 {
    let value: u64;
    // SAFETY: TPIDR_EL1 is readable at EL1.
    unsafe { asm!("mrs {}, TPIDR_EL1", out(reg) value, options(nomem, nostack)) };
    value
}

/// Set `TPIDR_EL1`. The caller owns the pointed-to per-core record.
pub fn set_tpidr_el1(value: u64) {
    // SAFETY: TPIDR_EL1 is writable at EL1.
    unsafe { asm!("msr TPIDR_EL1, {}", in(reg) value, options(nomem, nostack)) };
}

/// Request a platform power-off through PSCI. Returns the PSCI error code when
/// firmware rejects the request; on success firmware does not return.
///
/// # Safety
///
/// `use_hvc` must reflect the FADT ARM boot flags. Calling the wrong conduit
/// traps into the hypervisor or firmware with an unknown function.
pub unsafe fn psci_system_off(use_hvc: bool) -> Result<(), i64> {
    psci::system_off(use_hvc)
}

/// Stop the core forever. IRQs are masked first so a timer tick cannot keep
/// waking a core that has already finished its work.
pub fn halt() -> ! {
    // SAFETY: masking DAIF is local to this core.
    unsafe { asm!("msr DAIFSet, #0xf", options(nomem, nostack)) };
    loop {
        // SAFETY: WFE is a hint instruction with no memory side effects.
        unsafe { asm!("wfe", options(nomem, nostack)) };
    }
}
