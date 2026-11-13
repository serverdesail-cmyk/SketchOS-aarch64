//! GTDT (Generic Timer Description Table) decoding.
//!
//! The layout is the ACPI 6.5 one, which places the counter read base *after*
//! the four timer interrupt descriptors:
//!
//! ```text
//! 36  Counter Control Base        (8)
//! 44  Reserved                    (4)
//! 48  Secure EL1 GSIV + flags     (8)
//! 56  Non-secure EL1 GSIV + flags (8)
//! 64  Virtual EL1 GSIV + flags    (8)
//! 72  Non-secure EL2 GSIV + flags (8)
//! 80  Counter Read Base           (8)
//! 88  Platform Timer Count        (4)
//! 92  Platform Timer Offset       (4)
//! 96  Virtual EL2 GSIV + flags    (8)
//! ```
//!
//! This matches both the QEMU generator and the laptop firmware dump, so the
//! parser reads that ordering rather than the older field order some vendors
//! shipped.

use super::reader::Table;
use super::{Platform, Timer};

pub fn parse(table: &Table<'_>, platform: &mut Platform) {
    if table.length() < 96 {
        return;
    }

    let timer = Timer {
        control_base: table.u64(36),
        read_base: table.u64(80),
        secure_el1_gsiv: table.u32(48),
        secure_el1_flags: table.u32(52),
        non_secure_el1_gsiv: table.u32(56),
        non_secure_el1_flags: table.u32(60),
        virtual_el1_gsiv: table.u32(64),
        virtual_el1_flags: table.u32(68),
        non_secure_el2_gsiv: table.u32(72),
        non_secure_el2_flags: table.u32(76),
        platform_timers: table.u32(88),
    };

    // A table where every interrupt is absent is still a valid empty timer
    // description; keep it so diagnostics can print the raw values.
    platform.timer = Some(timer);
}
