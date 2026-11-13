//! Per-core data reachable through `TPIDR_EL1`.
//!
//! The first twelve fields are read by the assembly secondary-entry stub before
//! the MMU is enabled, so their offsets are an ABI. They are `AtomicU64` only
//! because the records live in a shared static; the assembly reads the same
//! eight bytes with an ordinary load after the boot core has cleaned them to
//! the point of coherency.
//!
//! `stack_top` is a *physical* address because the stub installs it before the
//! MMU is on. `entry` and `vbar` are high virtual addresses because the stub
//! branches to Rust and installs the vector table after the switch. `bias` is
//! `kernel_va_start - kernel_phys_start`, used to move the stack into the
//! higher half.
//!
//! Every other field is either core-local state (only the owning core writes)
//! or an atomic observed by the boot core for reporting. No core ever writes
//! another core's scheduler state.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::arch::aarch64::tpidr_el1;

/// Cores this milestone can start. The laptop has eight enabled cores; QEMU
/// tests use four.
pub const MAX_SMP_CORES: usize = 16;

/// Per-core startup stack.
pub const STACK_SIZE: usize = 16 * 1024;

/// Startup status values.
pub const STATUS_IDLE: u32 = 0;
pub const STATUS_STARTING: u32 = 1;
pub const STATUS_ONLINE: u32 = 2;
pub const STATUS_FAILED: u32 = 3;
pub const STATUS_TIMEOUT: u32 = 4;

#[repr(C, align(64))]
pub struct PerCpu {
    /// Stack top installed by the secondary stub. Offset 0.
    pub stack_top: AtomicU64,
    /// Translation state installed by the secondary stub.
    pub ttbr0: AtomicU64,
    pub tcr: AtomicU64,
    pub mair: AtomicU64,
    pub sctlr: AtomicU64,
    pub cpacr: AtomicU64,
    pub vbar: AtomicU64,
    /// Logical index assigned by the kernel, not a hardware identifier.
    pub index: AtomicU64,
    /// MADT/PSCI affinity value.
    pub mpidr: AtomicU64,
    /// Rust entry point called after the MMU is enabled (high virtual).
    pub entry: AtomicU64,
    /// Higher-half TTBR1 root installed by the secondary stub.
    pub ttbr1: AtomicU64,
    /// `kernel_va_start - kernel_phys_start`.
    pub bias: AtomicU64,

    pub status: AtomicU32,
    pub error: AtomicU32,
    pub started_at: AtomicU64,
    pub online_at: AtomicU64,
    pub gicr_base: AtomicU64,
    pub ipi_pings: AtomicU64,
    pub ipi_pongs: AtomicU64,
    pub ipi_latency_ticks: AtomicU64,
    pub work: AtomicU64,
    pub sched_ticks: AtomicU64,
    pub sched_switches: AtomicU64,
    pub sched_current: AtomicU32,
    pub sched_done: AtomicBool,
}

impl PerCpu {
    pub const EMPTY: Self = Self {
        stack_top: AtomicU64::new(0),
        ttbr0: AtomicU64::new(0),
        tcr: AtomicU64::new(0),
        mair: AtomicU64::new(0),
        sctlr: AtomicU64::new(0),
        cpacr: AtomicU64::new(0),
        vbar: AtomicU64::new(0),
        index: AtomicU64::new(0),
        mpidr: AtomicU64::new(0),
        entry: AtomicU64::new(0),
        ttbr1: AtomicU64::new(0),
        bias: AtomicU64::new(0),
        status: AtomicU32::new(STATUS_IDLE),
        error: AtomicU32::new(0),
        started_at: AtomicU64::new(0),
        online_at: AtomicU64::new(0),
        gicr_base: AtomicU64::new(0),
        ipi_pings: AtomicU64::new(0),
        ipi_pongs: AtomicU64::new(0),
        ipi_latency_ticks: AtomicU64::new(0),
        work: AtomicU64::new(0),
        sched_ticks: AtomicU64::new(0),
        sched_switches: AtomicU64::new(0),
        sched_current: AtomicU32::new(0),
        sched_done: AtomicBool::new(false),
    };
}

static RECORDS: [PerCpu; MAX_SMP_CORES] = [const { PerCpu::EMPTY }; MAX_SMP_CORES];
static BOOT_INDEX: AtomicU32 = AtomicU32::new(0);

const _: () = assert!(core::mem::size_of::<AtomicU64>() == 8);
const _: () = assert!(core::mem::offset_of!(PerCpu, stack_top) == 0);
const _: () = assert!(core::mem::offset_of!(PerCpu, ttbr0) == 8);
const _: () = assert!(core::mem::offset_of!(PerCpu, tcr) == 16);
const _: () = assert!(core::mem::offset_of!(PerCpu, mair) == 24);
const _: () = assert!(core::mem::offset_of!(PerCpu, sctlr) == 32);
const _: () = assert!(core::mem::offset_of!(PerCpu, cpacr) == 40);
const _: () = assert!(core::mem::offset_of!(PerCpu, vbar) == 48);
const _: () = assert!(core::mem::offset_of!(PerCpu, index) == 56);
const _: () = assert!(core::mem::offset_of!(PerCpu, mpidr) == 64);
const _: () = assert!(core::mem::offset_of!(PerCpu, entry) == 72);
const _: () = assert!(core::mem::offset_of!(PerCpu, ttbr1) == 80);
const _: () = assert!(core::mem::offset_of!(PerCpu, bias) == 88);

/// The record for the executing core. Every core installs `TPIDR_EL1` before
/// it runs Rust code.
pub fn current() -> &'static PerCpu {
    let pointer = tpidr_el1() as *const PerCpu;
    // SAFETY: the boot core sets TPIDR_EL1 in `smp::prepare` and each secondary
    // stub sets it from the PSCI context before calling Rust.
    unsafe { &*pointer }
}

// Query helper kept for callers that need the per-core record by index.
#[allow(dead_code)]
pub fn current_opt() -> Option<&'static PerCpu> {
    let pointer = tpidr_el1() as *const PerCpu;
    if pointer.is_null() {
        None
    } else {
        // SAFETY: see `current`.
        Some(unsafe { &*pointer })
    }
}

pub fn current_index() -> usize {
    current().index.load(Ordering::Relaxed) as usize
}

pub fn record(index: usize) -> Option<&'static PerCpu> {
    RECORDS.get(index)
}

// Iteration helper kept for diagnostics that inspect every core record.
#[allow(dead_code)]
pub fn records() -> &'static [PerCpu] {
    &RECORDS
}

pub fn set_boot_index(index: usize) {
    BOOT_INDEX.store(index as u32, Ordering::Release);
}

pub fn boot_index() -> usize {
    BOOT_INDEX.load(Ordering::Acquire) as usize
}

pub fn boot() -> &'static PerCpu {
    // SAFETY: `set_boot_index` is called before any core is started.
    unsafe { RECORDS.get_unchecked(boot_index()) }
}
