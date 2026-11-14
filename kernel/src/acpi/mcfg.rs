//! MCFG (PCIe memory-mapped configuration space) decoding.
//!
//! Each allocation entry describes one ECAM window: the physical base of the
//! configuration space for a PCI segment and the bus range it covers.

use super::reader::Table;
use super::{Mcfg, Platform, MAX_MCFG};

const HEADER: usize = 36;
const RESERVED: usize = 8;
const ENTRY_START: usize = HEADER + RESERVED;
const ENTRY_LENGTH: usize = 16;

pub fn parse(table: &Table<'_>, platform: &mut Platform) {
    let mut offset = ENTRY_START;
    while offset + ENTRY_LENGTH <= table.length() {
        if platform.mcfg_count >= MAX_MCFG {
            return;
        }
        let base = table.u64(offset);
        let segment = table.u16(offset + 8);
        let start_bus = table.u8(offset + 10);
        let end_bus = table.u8(offset + 11);
        if base == 0 || start_bus > end_bus {
            offset += ENTRY_LENGTH;
            continue;
        }

        // One ECAM bus occupies 1 MiB and the kernel's current linear
        // physical map covers the first 512 GiB only. MCFG bases must be
        // 1 MiB-aligned; reject malformed/out-of-map windows before any MMIO
        // configuration-space read can be attempted.
        let bus_count = (end_bus as u64) - (start_bus as u64) + 1;
        let window_bytes = match bus_count.checked_mul(1 << 20) {
            Some(bytes) => bytes,
            None => {
                offset += ENTRY_LENGTH;
                continue;
            }
        };
        let window_end = match base.checked_add(window_bytes) {
            Some(end) => end,
            None => {
                offset += ENTRY_LENGTH;
                continue;
            }
        };
        if base & ((1 << 20) - 1) != 0 || window_end > (512u64 << 30) {
            offset += ENTRY_LENGTH;
            continue;
        }
        platform.mcfg[platform.mcfg_count] = Mcfg {
            base,
            segment,
            start_bus,
            end_bus,
        };
        platform.mcfg_count += 1;
        offset += ENTRY_LENGTH;
    }
}
