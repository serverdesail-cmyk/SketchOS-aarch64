//! One-shot generic timer for the EL1 virtual timer.
//!
//! The scheduler decides when it next needs to run and programs `CNTV_CVAL_EL0`
//! for exactly that instant. There is no periodic tick: an idle core can mask
//! the timer and execute `WFI`, and a busy core is interrupted only when its
//! current slice request or a timer wait expires.
//!
//! `CNTVCT_EL0` is the (possibly offset) virtual counter the compare register is checked
//! against. The virtual counter is the one WHPX exposes to guests; using it for both\r\n//! reading and comparison keeps deadlines consistent under a hypervisor.
//!
//! This module does not use the GTDT memory-mapped timer frames: the
//! architecture's system-register timer is always available at EL1.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::arch::aarch64::counter_frequency;
use crate::println;

/// The shortest interval the scheduler will program. A zero or past deadline
/// would keep the level-sensitive PPI asserted and storm the core.
pub const MIN_EVENT_NS: u64 = 100_000;

/// `cntv_ctl_el0`: interrupt masked (timer still counts).
const CNTV_CTL_IMASK: u64 = 1 << 1;
/// `cntv_ctl_el0`: timer enabled, interrupt unmasked.
const CNTV_CTL_ENABLE: u64 = 1 << 0;

static READY: AtomicBool = AtomicBool::new(false);
static ARMED: AtomicBool = AtomicBool::new(false);
static LAST_DEADLINE_NS: AtomicU64 = AtomicU64::new(0);
static ARMED_EVENTS: AtomicU64 = AtomicU64::new(0);
static DISARMS: AtomicU64 = AtomicU64::new(0);

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

    // SAFETY: CNTV_*_EL0 are accessible at EL1 on a non-secure kernel. Masking
    // first stops any firmware-programmed deadline before CVAL changes.
    unsafe {
        asm!("msr cntv_ctl_el0, {}", in(reg) CNTV_CTL_IMASK, options(nomem, nostack));
        asm!("msr cntv_cval_el0, {}", in(reg) u64::MAX, options(nomem, nostack));
        asm!("isb", options(nomem, nostack));
    }
    ARMED.store(false, Ordering::Release);
    READY.store(true, Ordering::Release);
    Ok(())
}

/// Current physical counter value in nanoseconds.
pub fn now_ns() -> u64 {
    let ticks: u64;
    // SAFETY: CNTVCT_EL0 is readable at EL1 and is the counter CNTV_CVAL is
    // compared against.
    unsafe { asm!("mrs {}, CNTVCT_EL0", out(reg) ticks, options(nomem, nostack)) };
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
    // program the compare value, then enable.
    unsafe {
        asm!("msr cntv_ctl_el0, {}", in(reg) CNTV_CTL_IMASK, options(nomem, nostack));
        asm!("msr cntv_cval_el0, {}", in(reg) target, options(nomem, nostack));
        asm!("msr cntv_ctl_el0, {}", in(reg) CNTV_CTL_ENABLE, options(nomem, nostack));
        asm!("isb", options(nomem, nostack));
    }
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
    // SAFETY: cntv_ctl_el0 is writable at EL1 on the current core.
    unsafe {
        asm!("msr cntv_ctl_el0, {}", in(reg) CNTV_CTL_IMASK, options(nomem, nostack));
        asm!("isb", options(nomem, nostack));
    }
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
        "Timer: CNTV one-shot, {} Hz counter, {} ns resolution",
        frequency(),
        resolution_ns()
    );
}

/// Kept for callers that want the legacy tick-rate description.
#[allow(dead_code)]
pub const HZ: u64 = 1000;
