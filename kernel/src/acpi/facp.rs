//! FACP (FADT) decoding.
//!
//! Only the fields that change kernel behaviour are decoded: the DSDT
//! address (for later AML evaluation) and the ARM boot architecture flags
//! that signal PSCI support and the conduit (SMC or HVC).

use super::reader::Table;
use super::{Platform, Psci};

pub fn parse(table: &Table<'_>, platform: &mut Platform) {
    let x_dsdt = table.u64(140);
    let dsdt = table.u32(40) as u64;
    platform.dsdt_address = if x_dsdt != 0 { x_dsdt } else { dsdt };

    // ACPI 6.5, section 5.2.9: ARM Boot Architecture Flags are a 16-bit
    // field at offset 129. Bit 0 = PSCI compliant, bit 1 = use HVC.
    let arm_flags = table.u16(129);
    let compliant = arm_flags & 1 != 0;
    let use_hvc = compliant && arm_flags & 2 != 0;
    platform.psci = Psci {
        fadt_present: true,
        compliant,
        use_hvc,
    };
}
