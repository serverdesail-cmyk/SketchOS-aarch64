//! SMP discovery, PSCI startup, per-core initialization and IPIs.
//!
//! # Identity
//!
//! MADT `GICC` entries carry an MPIDR affinity value. The kernel assigns its
//! own logical index (boot core first) and keeps the hardware affinity in
//! [`PerCpu::mpidr`]. PSCI `CPU_ON` receives that affinity value, never the
//! logical index.
//!
//! # Startup
//!
//! The boot core writes each secondary's stack top and translation state into
//! its [`PerCpu`] record, cleans the kernel image to the point of coherency,
//! then calls PSCI `CPU_ON` with the physical address of `secondary_entry` and
//! the record address as context. The assembly stub installs the stack, MMU
//! registers and `TPIDR_EL1`, enables the MMU, and branches to
//! [`secondary_main`]. The secondary initializes its GIC redistributor and
//! timer, marks itself online, and waits for the scheduler release.
//!
//! Startup is bounded: every core gets a counter-based timeout. A core that
//! fails PSCI, fails GIC setup, or never reports is marked and excluded from
//! the scheduler. The boot core never waits forever.
//!
//! # IPIs
//!
//! GICv3 SGIs use `ICC_SGI1R_EL1`. SGI 0 is a ping to a secondary; SGI 1 is
//! the pong back to the boot core. Each ping is sent individually so the
//! round-trip latency is unambiguous. The pong handler runs in IRQ context and
//! only touches per-core atomics.

use core::arch::asm;
use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};

use sketchos_bootinfo::{BootInfo, KERNEL_VA_BASE};

use crate::acpi::{self, MPIDR_AFFINITY_MASK, Platform};
use crate::arch::aarch64::{cache, counter, counter_frequency, gic, mmu, mpidr, timer};
use crate::percpu::{self, PerCpu, MAX_SMP_CORES, STACK_SIZE};
use crate::println;

/// PSCI returned a negative error or is unavailable.
const PSCI_UNAVAILABLE: u32 = u32::MAX;

static CPU_COUNT: AtomicU32 = AtomicU32::new(1);
static EXPECTED: AtomicU32 = AtomicU32::new(0);
static STARTED: AtomicU32 = AtomicU32::new(1);
static BOOT_MPIDR: AtomicU64 = AtomicU64::new(0);
static PSCI_READY: AtomicBool = AtomicBool::new(false);
static PSCI_HVC: AtomicBool = AtomicBool::new(false);
static QEMU_VIRT: AtomicBool = AtomicBool::new(false);
static SCHED_GO: AtomicBool = AtomicBool::new(false);
/// Timestamp of the one outstanding ping. Only the boot core writes it, and it
/// waits for the pong before starting another ping, so a pong is never
/// attributed to the wrong core.
static IPI_SENT_AT: AtomicU64 = AtomicU64::new(0);
static IPI_PONG_TOTAL: AtomicU64 = AtomicU64::new(0);
static IPI_LAST_LATENCY: AtomicU64 = AtomicU64::new(0);
static PSCI_LAST_ERROR: AtomicI64 = AtomicI64::new(0);

#[repr(C, align(16))]
struct Stack([u8; STACK_SIZE]);

/// Wrapped in [`UnsafeCell`] so the stacks are emitted in `.bss`: they are used
/// as live stack memory and the final kernel map does not make `.rodata`
/// writable.
struct StackSet(UnsafeCell<[Stack; MAX_SMP_CORES]>);

// SAFETY: each secondary uses only its own slot, and PSCI starts it only after
// the boot core has published that slot's stack top.
unsafe impl Sync for StackSet {}

static SECONDARY_STACKS: StackSet =
    StackSet(UnsafeCell::new([const { Stack([0; STACK_SIZE]) }; MAX_SMP_CORES]));

extern "C" {
    /// Physical entry point installed in PSCI `CPU_ON`.
    fn secondary_entry();
}

/// Discover enabled MADT processors and build the per-core records.
///
/// Must run after the MMU is enabled and before the GIC is initialized.
pub fn discover(platform: &Platform) -> Result<(), &'static str> {
    let boot_mpidr = mpidr() & MPIDR_AFFINITY_MASK;
    BOOT_MPIDR.store(boot_mpidr, Ordering::Release);
    percpu::set_boot_index(0);

    let boot = percpu::record(0).ok_or("no per-core record storage")?;
    boot.index.store(0, Ordering::Relaxed);
    boot.mpidr.store(boot_mpidr, Ordering::Relaxed);
    boot.status.store(percpu::STATUS_ONLINE, Ordering::Release);
    boot.online_at.store(counter(), Ordering::Relaxed);
    boot.entry.store(secondary_main as *const () as usize as u64, Ordering::Relaxed);
    boot.sched_done.store(false, Ordering::Relaxed);
    boot.sched_current.store(0, Ordering::Relaxed);

    let mut count = 1usize;
    for cpu in platform.cpus[..platform.cpu_count].iter() {
        if !cpu.enabled {
            continue;
        }
        let affinity = cpu.mpidr & MPIDR_AFFINITY_MASK;
        if affinity == boot_mpidr {
            continue;
        }
        if count >= MAX_SMP_CORES {
            println!(
                "SMP: more than {} enabled CPUs; ignoring the remainder",
                MAX_SMP_CORES
            );
            break;
        }
        if (0..count).any(|index| {
            percpu::record(index)
                .map(|record| record.mpidr.load(Ordering::Relaxed) == affinity)
                .unwrap_or(false)
        }) {
            continue;
        }

        let record = percpu::record(count).ok_or("no per-core record storage")?;
        record.index.store(count as u64, Ordering::Relaxed);
        record.mpidr.store(affinity, Ordering::Relaxed);
        record.status.store(percpu::STATUS_IDLE, Ordering::Release);
        record.error.store(0, Ordering::Relaxed);
        record.stack_top.store(stack_top(count), Ordering::Relaxed);
        record.entry.store(secondary_main as *const () as usize as u64, Ordering::Relaxed);
        record.sched_done.store(false, Ordering::Relaxed);
        record.sched_current.store(0, Ordering::Relaxed);
        record.work.store(0, Ordering::Relaxed);
        record.ipi_pings.store(0, Ordering::Relaxed);
        record.ipi_pongs.store(0, Ordering::Relaxed);
        record.ipi_latency_ticks.store(0, Ordering::Relaxed);
        record.sched_ticks.store(0, Ordering::Relaxed);
        record.sched_switches.store(0, Ordering::Relaxed);
        count += 1;
    }

    CPU_COUNT.store(count as u32, Ordering::Release);
    EXPECTED.store((count - 1) as u32, Ordering::Release);
    STARTED.store(1, Ordering::Release);

    let psci = platform.psci.fadt_present && platform.psci.compliant;
    PSCI_READY.store(psci, Ordering::Release);
    PSCI_HVC.store(platform.psci.use_hvc, Ordering::Release);

    // The boot core must install its own TPIDR before any per-core helper is
    // called. The secondary stub does the same for each secondary.
    crate::arch::aarch64::set_tpidr_el1(boot as *const PerCpu as u64);
    Ok(())
}

/// Fill the translation state every secondary needs and publish the kernel
/// image to the point of coherency.
pub fn prepare(info: &BootInfo) -> Result<(), &'static str> {
    let state = mmu::state();
    let bias = info
        .kernel_va_start
        .checked_sub(info.kernel_phys_start)
        .ok_or("kernel virtual address precedes its physical address")?;
    let count = CPU_COUNT.load(Ordering::Acquire) as usize;
    for index in 0..count {
        let record = percpu::record(index).ok_or("per-core record disappeared")?;
        record.ttbr0.store(state.ttbr0, Ordering::Relaxed);
        record.ttbr1.store(state.ttbr1, Ordering::Relaxed);
        record.tcr.store(state.tcr, Ordering::Relaxed);
        record.mair.store(state.mair, Ordering::Relaxed);
        record.sctlr.store(state.sctlr, Ordering::Relaxed);
        record.cpacr.store(state.cpacr, Ordering::Relaxed);
        record.vbar.store(state.vbar, Ordering::Relaxed);
        record.bias.store(bias, Ordering::Relaxed);
        record.entry.store(secondary_main as *const () as usize as u64, Ordering::Relaxed);
    }

    // The secondary starts with the MMU and caches off. Its entry stub, the
    // page tables, its stack and its record must all be in memory, not only in
    // the boot core's data cache.
    cache::clean_dcache_range(info.kernel_phys_start, info.kernel_phys_end);
    cache::invalidate_icache_range(info.kernel_phys_start, info.kernel_phys_end);
    Ok(())
}

/// Start every discovered secondary and wait for it to report online.
pub fn start_secondaries() {
    let count = CPU_COUNT.load(Ordering::Acquire) as usize;
    if count <= 1 {
        return;
    }

    if !PSCI_READY.load(Ordering::Acquire) {
        for index in 1..count {
            if let Some(record) = percpu::record(index) {
                record.error.store(PSCI_UNAVAILABLE, Ordering::Relaxed);
                record.status.store(percpu::STATUS_FAILED, Ordering::Release);
            }
        }
        STARTED.store(1, Ordering::Release);
        return;
    }

    let use_hvc = PSCI_HVC.load(Ordering::Acquire);
    let timeout = counter_frequency().saturating_mul(2);
    let mut online = 1u32;

    for index in 1..count {
        let Some(record) = percpu::record(index) else {
            continue;
        };
        record.status.store(percpu::STATUS_STARTING, Ordering::Release);
        let started = counter();
        record.started_at.store(started, Ordering::Relaxed);

        let target = record.mpidr.load(Ordering::Relaxed);
        // PSCI must enter the assembly stub, not the Rust function. The stub
        // installs the stack and translation state and then branches to the
        // Rust entry stored in the record. Firmware starts the core with the
        // MMU off, so both the entry and the context it reads must be physical.
        let entry = (secondary_entry as *const () as usize as u64)
            .checked_sub(KERNEL_VA_BASE)
            .unwrap_or(0);
        let context = (record as *const PerCpu as u64)
            .checked_sub(KERNEL_VA_BASE)
            .unwrap_or(0);
        if entry == 0 || context == 0 {
            record.error.store(1, Ordering::Relaxed);
            record.status.store(percpu::STATUS_FAILED, Ordering::Release);
            continue;
        }
        match crate::arch::aarch64::psci::cpu_on(target, entry, context, use_hvc) {
            Ok(()) => {
                if wait_online(record, timeout) {
                    online += 1;
                }
            }
            Err(code) => {
                PSCI_LAST_ERROR.store(code, Ordering::Relaxed);
                record.error.store(code as u32, Ordering::Relaxed);
                record.status.store(percpu::STATUS_FAILED, Ordering::Release);
            }
        }
    }

    STARTED.store(online, Ordering::Release);
}

fn wait_online(record: &PerCpu, timeout: u64) -> bool {
    let start = counter();
    loop {
        match record.status.load(Ordering::Acquire) {
            percpu::STATUS_ONLINE => return true,
            percpu::STATUS_FAILED => return false,
            percpu::STATUS_TIMEOUT => return false,
            _ => {}
        }
        if counter().wrapping_sub(start) >= timeout {
            record.status.store(percpu::STATUS_TIMEOUT, Ordering::Release);
            return false;
        }
        spin_loop();
    }
}

/// Rust entry point for a secondary after the assembly stub enabled the MMU.
extern "C" fn secondary_main(record: *const PerCpu) -> ! {
    // SAFETY: the PSCI context is the address of this core's static record.
    let cpu = unsafe { &*record };

    // The stub has moved the stack into the higher half and the final TTBR1
    // root is live. TTBR0 still holds the bootstrap identity map; this core no
    // longer needs it.
    mmu::seal_low_half();

    let platform = match acpi::installed() {
        Some(platform) => platform,
        None => secondary_fail(cpu, 1),
    };

    match gic::init_cpu(platform, cpu.mpidr.load(Ordering::Relaxed)) {
        Ok(base) => cpu.gicr_base.store(base, Ordering::Relaxed),
        Err(_) => secondary_fail(cpu, 2),
    }

    // Start the private timer before enabling IRQs. A stale firmware deadline
    // must never reach the IRQ handler with no scheduler frames built.
    if timer::init().is_err() {
        secondary_fail(cpu, 3);
    }

    cpu.online_at.store(counter(), Ordering::Relaxed);
    cpu.status.store(percpu::STATUS_ONLINE, Ordering::Release);

    // IRQs are needed for IPIs while this core waits for the scheduler.
    gic::enable_irq();
    // Spin briefly on the release flag. This wait is short and bounded by the
    // boot core's scheduler setup; WFE would add a lost-wakeup window that
    // depends on a later timer interrupt to recover.
    while !SCHED_GO.load(Ordering::Acquire) {
        spin_loop();
    }

    crate::sched::start_core()
}

fn secondary_fail(cpu: &PerCpu, code: u32) -> ! {
    cpu.error.store(code, Ordering::Relaxed);
    cpu.status.store(percpu::STATUS_FAILED, Ordering::Release);
    // SAFETY: masking DAIF is local to this core.
    unsafe { asm!("msr DAIFSet, #0xf", options(nomem, nostack)) };
    crate::arch::halt()
}

/// Release every secondary into its scheduler.
pub fn release_scheduler() {
    SCHED_GO.store(true, Ordering::Release);
}

// Query helper kept for startup diagnostics.
#[allow(dead_code)]
pub fn scheduler_released() -> bool {
    SCHED_GO.load(Ordering::Acquire)
}

/// Record whether the platform is QEMU `virt`, so the demo only requests
/// SYSTEM_OFF on the emulated machine the verification scripts target.
pub fn set_qemu(qemu: bool) {
    QEMU_VIRT.store(qemu, Ordering::Release);
}

/// Return the PSCI conduit when a power-off request is appropriate. On real
/// hardware this milestone still halts instead of claiming a verified
/// power-off path.
pub fn power_off() -> Option<bool> {
    if QEMU_VIRT.load(Ordering::Acquire) && PSCI_READY.load(Ordering::Acquire) {
        Some(PSCI_HVC.load(Ordering::Acquire))
    } else {
        None
    }
}

/// Return the firmware-declared PSCI conduit for an explicit user request.
/// Unlike the automated smoke-test helper, this is not restricted to QEMU.
pub fn explicit_power_off() -> Option<bool> {
    if PSCI_READY.load(Ordering::Acquire) {
        Some(PSCI_HVC.load(Ordering::Acquire))
    } else {
        None
    }
}

/// Run a ping/pong round trip against every online secondary.
///
/// Only one ping is outstanding at a time. If a secondary does not answer
/// before the bounded timeout, the test fails instead of pinging the next core:
/// a pong carries no sender identity, so a late reply must never be credited to
/// another core.
pub fn run_ipi_test() -> Result<u32, &'static str> {
    let count = CPU_COUNT.load(Ordering::Acquire) as usize;
    if count <= 1 {
        return Ok(0);
    }

    let mut tested = 0u32;
    gic::enable_irq();

    for index in 1..count {
        let Some(record) = percpu::record(index) else {
            continue;
        };
        if record.status.load(Ordering::Acquire) != percpu::STATUS_ONLINE {
            continue;
        }
        let before = IPI_PONG_TOTAL.load(Ordering::Acquire);
        let sent = counter();
        IPI_SENT_AT.store(sent, Ordering::Relaxed);
        if gic::send_sgi(record.mpidr.load(Ordering::Relaxed), gic::SGI_PING).is_err() {
            gic::disable_irq();
            return Err("failed to send an IPI to an online secondary");
        }

        let timeout = counter_frequency() / 5;
        let mut answered = false;
        while counter().wrapping_sub(sent) < timeout {
            if IPI_PONG_TOTAL.load(Ordering::Acquire) > before {
                let latency = IPI_LAST_LATENCY.load(Ordering::Acquire);
                record.ipi_pongs.fetch_add(1, Ordering::Relaxed);
                record.ipi_latency_ticks.store(latency, Ordering::Relaxed);
                tested += 1;
                println!(
                    "  IPI core {} (MPIDR 0x{:016x}): {} ticks round trip",
                    index,
                    record.mpidr.load(Ordering::Relaxed),
                    latency
                );
                answered = true;
                break;
            }
            spin_loop();
        }
        if !answered {
            gic::disable_irq();
            return Err("an online secondary did not answer an IPI");
        }
    }

    gic::disable_irq();
    if tested == 0 {
        Err("no secondary answered an IPI")
    } else {
        Ok(tested)
    }
}

/// SGI handler called from the IRQ path.
pub fn on_sgi(intid: u32) {
    let cpu = percpu::current();
    match intid {
        gic::SGI_PING => {
            cpu.ipi_pings.fetch_add(1, Ordering::Relaxed);
            let boot = percpu::boot().mpidr.load(Ordering::Relaxed);
            let _ = gic::send_sgi(boot, gic::SGI_PONG);
        }
        gic::SGI_PONG => {
            // The pong is handled on the boot core. Record only the latency
            // here; `run_ipi_test` attributes it to the core it just pinged.
            let latency = counter().wrapping_sub(IPI_SENT_AT.load(Ordering::Relaxed));
            IPI_LAST_LATENCY.store(latency, Ordering::Release);
            IPI_PONG_TOTAL.fetch_add(1, Ordering::Release);
        }
        _ => {}
    }
}

pub fn mark_sched_done() {
    percpu::current().sched_done.store(true, Ordering::Release);
}

pub fn all_sched_done() -> bool {
    let count = CPU_COUNT.load(Ordering::Acquire) as usize;
    for index in 1..count {
        if let Some(record) = percpu::record(index) {
            if record.status.load(Ordering::Acquire) == percpu::STATUS_ONLINE
                && !record.sched_done.load(Ordering::Acquire)
            {
                return false;
            }
        }
    }
    true
}

/// Wait, with a bounded timeout, for every online secondary to finish its
/// scheduler demo. Called by the boot core after it left the timer IRQ path, so
/// interrupts stay enabled while it waits. Returns false on timeout.
pub fn wait_for_schedulers() -> bool {
    let timeout = counter_frequency().saturating_mul(2);
    let start = counter();
    while !all_sched_done() {
        if counter().wrapping_sub(start) >= timeout {
            println!("SMP: timeout waiting for secondary schedulers");
            return false;
        }
        spin_loop();
    }
    true
}

pub fn online_count() -> u32 {
    STARTED.load(Ordering::Acquire)
}

pub fn expected_count() -> u32 {
    EXPECTED.load(Ordering::Acquire) + 1
}

// Query helper kept for diagnostics that need the discovered CPU count.
#[allow(dead_code)]
pub fn cpu_count() -> usize {
    CPU_COUNT.load(Ordering::Acquire) as usize
}

pub fn concurrency_verified() -> bool {
    let count = CPU_COUNT.load(Ordering::Acquire) as usize;
    let mut online = 0u32;
    for index in 0..count {
        let Some(record) = percpu::record(index) else {
            continue;
        };
        let is_online =
            index == 0 || record.status.load(Ordering::Acquire) == percpu::STATUS_ONLINE;
        if !is_online {
            continue;
        }
        online += 1;
        if record.work.load(Ordering::Acquire) == 0 {
            return false;
        }
        if record.sched_ticks.load(Ordering::Acquire) == 0 {
            return false;
        }
        // A secondary only reports completion after its demo ended. Requiring
        // this bit means a timeout can never be printed as "verified".
        if index != percpu::boot_index() && !record.sched_done.load(Ordering::Acquire) {
            return false;
        }
    }
    online >= 2
}

/// Print discovery and startup state.
pub fn report() {
    let count = CPU_COUNT.load(Ordering::Acquire) as usize;
    let expected = expected_count();
    println!(
        "SMP: {} discovered, {} expected online, boot MPIDR 0x{:016x}, PSCI {} ({})",
        count,
        expected,
        BOOT_MPIDR.load(Ordering::Acquire),
        if PSCI_READY.load(Ordering::Acquire) {
            "compliant"
        } else {
            "unavailable"
        },
        if PSCI_HVC.load(Ordering::Acquire) {
            "HVC"
        } else {
            "SMC"
        }
    );
    for index in 0..count {
        let Some(record) = percpu::record(index) else {
            continue;
        };
        let status = record.status.load(Ordering::Acquire);
        let name = match status {
            percpu::STATUS_ONLINE => "online",
            percpu::STATUS_STARTING => "starting",
            percpu::STATUS_FAILED => "failed",
            percpu::STATUS_TIMEOUT => "timeout",
            _ => "idle",
        };
        let latency = startup_us(record);
        if index == 0 {
            println!(
                "  core 0: boot, MPIDR 0x{:016x}, online",
                record.mpidr.load(Ordering::Relaxed)
            );
        } else {
            println!(
                "  core {}: {}, MPIDR 0x{:016x}, startup {} us, GICR 0x{:016x}",
                index,
                name,
                record.mpidr.load(Ordering::Relaxed),
                latency,
                record.gicr_base.load(Ordering::Relaxed)
            );
        }
    }
    if PSCI_LAST_ERROR.load(Ordering::Relaxed) != 0 {
        println!(
            "  PSCI last error: {}",
            PSCI_LAST_ERROR.load(Ordering::Relaxed)
        );
    }
}

/// Print per-core execution and IPI results after the scheduler demo.
pub fn report_results() {
    let count = CPU_COUNT.load(Ordering::Acquire) as usize;
    let mut total_work = 0u64;
    for index in 0..count {
        if let Some(record) = percpu::record(index) {
            total_work = total_work.saturating_add(record.work.load(Ordering::Relaxed));
        }
    }
    println!(
        "SMP: {} of {} CPUs online, total work {}",
        online_count(),
        expected_count(),
        total_work
    );
    for index in 0..count {
        let Some(record) = percpu::record(index) else {
            continue;
        };
        let status = record.status.load(Ordering::Acquire);
        if index != 0 && status != percpu::STATUS_ONLINE {
            println!(
                "  core {}: {} (error {})",
                index,
                if status == percpu::STATUS_TIMEOUT {
                    "timeout"
                } else {
                    "failed"
                },
                record.error.load(Ordering::Relaxed)
            );
            continue;
        }
        println!(
            "  core {}: work {}, ticks {}, switches {}, IPI pings {}, pongs {}, latency {} ticks",
            index,
            record.work.load(Ordering::Relaxed),
            record.sched_ticks.load(Ordering::Relaxed),
            record.sched_switches.load(Ordering::Relaxed),
            record.ipi_pings.load(Ordering::Relaxed),
            record.ipi_pongs.load(Ordering::Relaxed),
            record.ipi_latency_ticks.load(Ordering::Relaxed)
        );
    }
}

fn startup_us(record: &PerCpu) -> u64 {
    let started = record.started_at.load(Ordering::Relaxed);
    let online = record.online_at.load(Ordering::Relaxed);
    let frequency = counter_frequency();
    if online <= started || frequency == 0 {
        return 0;
    }
    let ticks = online - started;
    (ticks as u128 * 1_000_000 / frequency as u128) as u64
}

fn stack_top(index: usize) -> u64 {
    // The static lives in the higher half; the stub installs this before the
    // MMU uses the kernel map, so it must be physical.
    // SAFETY: `index` is the calling core's own slot and is in range.
    let base = unsafe { (*SECONDARY_STACKS.0.get()).as_ptr().add(index) as u64 };
    let physical = base.checked_sub(KERNEL_VA_BASE).unwrap_or(base);
    physical + STACK_SIZE as u64
}
