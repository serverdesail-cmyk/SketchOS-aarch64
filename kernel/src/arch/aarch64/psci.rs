//! PSCI calls through the conduit the FADT declares.
//!
//! The FADT ARM boot flags select SMC or HVC. This module never guesses a
//! conduit: it uses the conduit the FADT declares. `PSCI_VERSION` is queried
//! before `CPU_ON` so a 0.1 implementation gets the 32-bit function. Functions
//! with only a 32-bit form, such as `SYSTEM_OFF`, always use their 32-bit ID.
//!
//! Every call runs with interrupts masked from the caller's context. The
//! assembly trampoline preserves the link register so firmware cannot return
//! into an unknown state.

/// `PSCI_VERSION` is always the 32-bit function.
const PSCI_VERSION: u64 = 0x8400_0000;
const PSCI_CPU_ON_32: u64 = 0x8400_0003;
const PSCI_CPU_ON_64: u64 = 0xC400_0003;
/// `SYSTEM_OFF` has only a 32-bit function ID in every PSCI version. A 64-bit
/// ID such as `0xC4000008` is not defined and firmware rejects it.
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;

extern "C" {
    fn psci_call_asm(function: u64, arg1: u64, arg2: u64, arg3: u64, use_hvc: u64) -> i64;
}

/// Raw PSCI call. `use_hvc` selects HVC when true, SMC when false.
pub fn call(function: u64, arg1: u64, arg2: u64, arg3: u64, use_hvc: bool) -> i64 {
    // SAFETY: the caller supplies a function ID and arguments from firmware
    // tables (FADT/PSCI) and the assembly preserves the return path.
    unsafe { psci_call_asm(function, arg1, arg2, arg3, use_hvc as u64) }
}

/// `PSCI_VERSION`, or a negative error code.
pub fn version(use_hvc: bool) -> Result<u32, i64> {
    let result = call(PSCI_VERSION, 0, 0, 0, use_hvc);
    if result < 0 {
        Err(result as i32 as i64)
    } else {
        Ok(result as u32)
    }
}

/// Start `target` at `entry` with `context` in `x0`.
///
/// `target` is the MPIDR affinity value from MADT, not a logical CPU index.
pub fn cpu_on(
    target: u64,
    entry: u64,
    context: u64,
    use_hvc: bool,
) -> Result<(), i64> {
    let version = version(use_hvc)?;
    let (function, wide) = if version >= 0x0000_0002 {
        (PSCI_CPU_ON_64, true)
    } else {
        (PSCI_CPU_ON_32, false)
    };
    let result = call(function, target, entry, context, use_hvc);
    if result == 0 {
        Ok(())
    } else {
        Err(if wide { result } else { result as i32 as i64 })
    }
}

/// Request platform power-off.
///
/// `SYSTEM_OFF` is a 32-bit PSCI function in all versions. On success the call
/// does not return; `Ok(())` therefore means firmware returned success anyway.
/// `Err` carries the PSCI error code, so the caller can report that the request
/// was rejected instead of pretending the machine powered off.
pub fn system_off(use_hvc: bool) -> Result<(), i64> {
    let result = call(PSCI_SYSTEM_OFF, 0, 0, 0, use_hvc);
    if result == 0 {
        Ok(())
    } else {
        Err(result as i32 as i64)
    }
}
