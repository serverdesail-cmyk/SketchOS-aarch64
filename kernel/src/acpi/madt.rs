//! MADT (APIC) decoding.
//!
//! The loader only needed the RSDP. The kernel decodes the interrupt
//! controller topology so later milestones can bring up GICv3, ITS and the
//! per-CPU redistributors without re-parsing firmware tables.
//!
//! Offsets follow ACPI 6.5, section 5.2.12. The GICC layout is the one the
//! laptop firmware actually emits (80 bytes, including the power efficiency
//! class at offset 76).

use super::reader::Table;
use super::{Cpu, Gicd, Gicr, Its, Platform, MAX_CPUS, MAX_GICR, MAX_ITS};

const HEADER: usize = 36;
const ENTRY_START: usize = HEADER + 8;

const TYPE_GICC: u8 = 0x0B;
const TYPE_GICD: u8 = 0x0C;
const TYPE_MSI_FRAME: u8 = 0x0D;
const TYPE_GICR: u8 = 0x0E;
const TYPE_ITS: u8 = 0x0F;

pub fn parse(table: &Table<'_>, platform: &mut Platform) {
    let mut offset = ENTRY_START;
    let length = table.length();

    while offset + 2 <= length {
        let kind = table.u8(offset);
        let entry_length = table.u8(offset + 1) as usize;
        // A zero or undersized entry length would loop forever. A length that
        // runs past the table means the table is truncated, so stop.
        if entry_length < 2 || offset.saturating_add(entry_length) > length {
            break;
        }

        // Validate each known structure's minimum ACPI-defined size before
        // reading fields from its payload. The outer table bounds check alone
        // does not prevent a short entry from borrowing bytes from the next.
        match kind {
            TYPE_GICC if entry_length >= 80 => parse_gicc(table, offset, platform),
            TYPE_GICD if entry_length >= 24 => parse_gicd(table, offset, platform),
            TYPE_GICR if entry_length >= 16 => parse_gicr(table, offset, platform),
            TYPE_ITS if entry_length >= 20 => parse_its(table, offset, platform),
            TYPE_MSI_FRAME if entry_length >= 24 => platform.gic_msi_frames += 1,
            _ => {}
        }

        offset += entry_length;
    }
}

fn parse_gicc(table: &Table<'_>, offset: usize, platform: &mut Platform) {
    if platform.cpu_count >= MAX_CPUS {
        return;
    }
    let flags = table.u32(offset + 12);
    let cpu = Cpu {
        interface: table.u32(offset + 4),
        uid: table.u32(offset + 8),
        mpidr: table.u64(offset + 68),
        gicc: table.u64(offset + 32),
        gicv: table.u64(offset + 40),
        gich: table.u64(offset + 48),
        gicr: table.u64(offset + 60),
        performance_gsiv: table.u32(offset + 20),
        vgic_gsiv: table.u32(offset + 56),
        enabled: flags & 1 != 0,
        efficiency: table.u8(offset + 76),
    };
    if cpu.enabled {
        platform.enabled_cpus += 1;
    }
    platform.cpus[platform.cpu_count] = cpu;
    platform.cpu_count += 1;
}

fn parse_gicd(table: &Table<'_>, offset: usize, platform: &mut Platform) {
    // Keep the first distributor. A system with several GICDs needs a
    // per-region model that this milestone does not have.
    platform.gicd.get_or_insert(Gicd {
        id: table.u32(offset + 4),
        base: table.u64(offset + 8),
        version: table.u8(offset + 20),
    });
}

fn parse_gicr(table: &Table<'_>, offset: usize, platform: &mut Platform) {
    if platform.gicr_count >= MAX_GICR {
        return;
    }
    platform.gicr[platform.gicr_count] = Gicr {
        base: table.u64(offset + 4),
        length: table.u32(offset + 12),
    };
    platform.gicr_count += 1;
}

fn parse_its(table: &Table<'_>, offset: usize, platform: &mut Platform) {
    if platform.its_count >= MAX_ITS {
        return;
    }
    platform.its[platform.its_count] = Its {
        id: table.u32(offset + 4),
        base: table.u64(offset + 8),
    };
    platform.its_count += 1;
}
