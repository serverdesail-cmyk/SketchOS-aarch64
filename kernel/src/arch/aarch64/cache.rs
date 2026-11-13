//! Cache maintenance for secondary-core startup.
//!
//! A PSCI `CPU_ON` target begins with its MMU and data cache disabled, so it
//! reads the entry stub, page tables, per-core records and stacks directly from
//! memory. The boot core must clean those writes to the point of coherency and
//! invalidate the instruction cache over the code the secondary will fetch.
//!
//! Line sizes come from `CTR_EL0`; nothing is hard-coded. The operations use
//! the inner-shareable domain, matching the Normal inner-shareable mapping the
//! kernel installs.

use core::arch::asm;

fn ctr_el0() -> u64 {
    let value: u64;
    // SAFETY: CTR_EL0 is readable at EL1.
    unsafe { asm!("mrs {}, CTR_EL0", out(reg) value, options(nomem, nostack)) };
    value
}

fn dcache_line_size() -> u64 {
    4 << ((ctr_el0() >> 16) & 0xf)
}

fn icache_line_size() -> u64 {
    4 << (ctr_el0() & 0xf)
}

/// Clean the data cache for `[start, end)` to the point of coherency.
pub fn clean_dcache_range(start: u64, end: u64) {
    if start >= end {
        return;
    }
    let line = dcache_line_size();
    let mut address = start & !(line - 1);
    while address < end {
        // SAFETY: DC CVAC is a cache-maintenance hint with no architectural
        // memory side effect beyond the cache operation.
        unsafe { asm!("dc cvac, {}", in(reg) address, options(nostack)) };
        address = address.saturating_add(line);
    }
    // SAFETY: DSB ISH completes the maintenance before the caller proceeds.
    unsafe { asm!("dsb ish", options(nostack)) };
}

/// Clean and invalidate the data cache for `[start, end)`.
// Kept as part of the cache API even though the 0.0.3 startup path only needs
// the clean operation.
#[allow(dead_code)]
pub fn clean_invalidate_dcache_range(start: u64, end: u64) {
    if start >= end {
        return;
    }
    let line = dcache_line_size();
    let mut address = start & !(line - 1);
    while address < end {
        // SAFETY: DC CIVAC is a cache-maintenance hint.
        unsafe { asm!("dc civac, {}", in(reg) address, options(nostack)) };
        address = address.saturating_add(line);
    }
    // SAFETY: DSB ISH completes the maintenance.
    unsafe { asm!("dsb ish", options(nostack)) };
}

/// Invalidate the whole instruction cache for this core.
///
/// User images are written through the physical map and then executed through a
/// different virtual alias, so 0.0.5 invalidates the entire I-cache once before
/// entering EL0 instead of relying on alias-specific maintenance.
pub fn invalidate_icache_all() {
    // SAFETY: IC IALLU is a local cache-maintenance operation available at EL1.
    unsafe {
        asm!("ic iallu", options(nostack));
        asm!("dsb ish", options(nostack));
        asm!("isb", options(nostack));
    }
}

/// Invalidate the instruction cache for `[start, end)`.
pub fn invalidate_icache_range(start: u64, end: u64) {
    if start >= end {
        return;
    }
    let line = icache_line_size();
    let mut address = start & !(line - 1);
    while address < end {
        // SAFETY: IC IVAU is a cache-maintenance hint.
        unsafe { asm!("ic ivau, {}", in(reg) address, options(nostack)) };
        address = address.saturating_add(line);
    }
    // SAFETY: DSB ISH and ISB make the new instructions visible to fetch.
    unsafe {
        asm!("dsb ish", options(nostack));
        asm!("isb", options(nostack));
    }
}
