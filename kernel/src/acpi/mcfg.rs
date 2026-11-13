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
