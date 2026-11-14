//! One-shot generic timer selected from the ACPI GTDT.
//!
//! The interrupt ID and timer register bank are selected as a matched pair:
//! Virtual EL1 GSIV uses CNTV_*; non-secure Physical EL1 GSIV uses CNTP_*.
//! This avoids enabling one PPI while programming the other timer source.
//! There is no periodic tick: idle cores mask their timer and execute WFI.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::arch::aarch64::{counter_frequency, gic::TimerKind};
use crate::println;

/// The shortest interval the scheduler will program. A zero or past deadline
/// would keep the level-sensitive PPI asserted and storm the core.
pub const MIN_EVENT_NS: u64 = 100_000;

/// `cntv_ctl_el0`: interrupt masked (timer still counts).
const TIMER_CTL_IMASK: u64 = 1 << 1;
/// `cntv_ctl_el0`: timer enabled, interrupt unmasked.
const TIMER_CTL_ENABLE: u64 = 1 << 0;

static READY: AtomicBool = AtomicBool::new(false);
static ARMED: AtomicBool = AtomicBool::new(false);
static LAST_DEADLINE_NS: AtomicU64 = AtomicU64::new(0);
static ARMED_EVENTS: AtomicU64 = AtomicU64::new(0);
static DISARMS: AtomicU64 = AtomicU64::new(0);

fn read_counter() -> u64 {
    let mut ticks: u64 = 0;
    // SAFETY: the GTDT-selected counter is enabled for non-secure EL1.
    unsafe {
        match crate::arch::aarch64::gic::timer_kind() {
            TimerKind::Virtual => {
                asm!("mrs {}, CNTVCT_EL0", out(reg) ticks, options(nomem, nostack));
            }
            TimerKind::Physical => {
                asm!("mrs {}, CNTPCT_EL0", out(reg) ticks, options(nomem, nostack));
            }
        }
    }
    ticks
}

fn write_control(value: u64) {
    // SAFETY: the GTDT-selected timer control register is writable at EL1.
    unsafe {
        match crate::arch::aarch64::gic::timer_kind() {
            TimerKind::Virtual => {
                asm!("msr CNTV_CTL_EL0, {}", in(reg) value, options(nomem, nostack));
            }
            TimerKind::Physical => {
                asm!("msr CNTP_CTL_EL0, {}", in(reg) value, options(nomem, nostack));
            }
        }
    }
}

fn write_compare(value: u64) {
    // SAFETY: the GTDT-selected timer compare register is writable at EL1.
    unsafe {
        match crate::arch::aarch64::gic::timer_kind() {
            TimerKind::Virtual => {
                asm!("msr CNTV_CVAL_EL0, {}", in(reg) value, options(nomem, nostack));
            }
            TimerKind::Physical => {
                asm!("msr CNTP_CVAL_EL0, {}", in(reg) value, options(nomem, nostack));
            }
        }
    }
}

fn frequency() -> u64 {
    counter_frequency().max(1)
}

/// Nanoseconds represented by one counter tick, rounded up.
fn ns_per_tick() -> u64 {
    let frequency = frequency();
    if frequency >= 1_000_000_000 {
        1
    } else {
        (1_000_000_000 + frequency - 1) / frequency
    }
}

/// Start the timer in a disarmed state. Every core programs its own
/// `CNTV_*_EL0` registers; the boot core's counter frequency is shared.
pub fn init() -> Result<(), &'static str> {
    let frequency = counter_frequency();
    if frequency == 0 {
        return Err("generic timer frequency is zero");
    }

    // SAFETY: the selected system-register timer is accessible at EL1.
    write_control(TIMER_CTL_IMASK);
    write_compare(u64::MAX);
    unsafe { asm!("isb", options(nomem, nostack)) };
    ARMED.store(false, Ordering::Release);
    READY.store(true, Ordering::Release);
    Ok(())
}

/// Current selected architectural counter value in nanoseconds.
pub fn now_ns() -> u64 {
    let ticks = read_counter();
    let frequency = frequency();
    ((ticks as u128 * 1_000_000_000u128) / frequency as u128) as u64
}

/// Arm the timer for an absolute nanosecond deadline.
///
/// A deadline at or before `now` is pushed forward by [`MIN_EVENT_NS`] so the
/// level-sensitive PPI is deasserted before the handler returns.
pub fn arm_at_ns(deadline_ns: u64) {
    if !READY.load(Ordering::Acquire) {
        return;
    }
    let now = now_ns();
    let deadline = if deadline_ns <= now {
        now.saturating_add(MIN_EVENT_NS)
    } else {
        deadline_ns
    };
    let frequency = frequency();
    let target = ((deadline as u128 * frequency as u128) / 1_000_000_000u128) as u64;

    // SAFETY: called by the owning core with the scheduler lock held. Mask,
    // program the selected compare value, then enable that timer source.
    write_control(TIMER_CTL_IMASK);
    write_compare(target);
    write_control(TIMER_CTL_ENABLE);
    unsafe { asm!("isb", options(nomem, nostack)) };
    LAST_DEADLINE_NS.store(deadline, Ordering::Relaxed);
    ARMED.store(true, Ordering::Release);
    ARMED_EVENTS.fetch_add(1, Ordering::Relaxed);
}

/// Arm the timer for `duration_ns` from now.
pub fn arm_ns(duration_ns: u64) {
    arm_at_ns(now_ns().saturating_add(duration_ns.max(MIN_EVENT_NS)));
}

/// Mask the timer interrupt. Called at the start of the timer IRQ handler so
/// the level-sensitive PPI is deasserted before end-of-interrupt.
pub fn disarm() {
    // SAFETY: the selected timer control register is writable at EL1.
    write_control(TIMER_CTL_IMASK);
    unsafe { asm!("isb", options(nomem, nostack)) };
    ARMED.store(false, Ordering::Release);
    DISARMS.fetch_add(1, Ordering::Relaxed);
}

/// True while a deadline is programmed.
pub fn is_armed() -> bool {
    ARMED.load(Ordering::Acquire)
}

/// True after a successful [`init`].
pub fn is_ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// Absolute nanosecond deadline most recently programmed.
pub fn last_deadline_ns() -> u64 {
    LAST_DEADLINE_NS.load(Ordering::Acquire)
}

pub fn armed_events() -> u64 {
    ARMED_EVENTS.load(Ordering::Relaxed)
}

pub fn disarm_count() -> u64 {
    DISARMS.load(Ordering::Relaxed)
}

/// Wait for the next interrupt. IRQs must be unmasked; the scheduler arms a
/// timer only when there is a real event.
pub fn wait_for_interrupt() {
    // SAFETY: WFI is a hint with no memory side effects. An unmasked interrupt
    // or debug event wakes the core.
    unsafe { asm!("wfi", options(nomem, nostack)) };
}

/// One-tick resolution in nanoseconds, for reporting only.
pub fn resolution_ns() -> u64 {
    ns_per_tick()
}

/// Print the negotiated timer information.
pub fn report() {
    println!(
        "Timer: {:?} one-shot PPI {}, {} Hz counter, {} ns resolution",
        crate::arch::aarch64::gic::timer_kind(),
        crate::arch::aarch64::gic::timer_irq(),
        frequency(),
        resolution_ns()
    );
}

/// Kept for callers that want the legacy tick-rate description.
#[allow(dead_code)]
pub const HZ: u64 = 1000;
