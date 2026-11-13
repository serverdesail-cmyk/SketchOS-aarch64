//! SRAT and SLIT decoding.
//!
//! SRAT supplies memory affinity ranges (and, on other platforms, CPU
//! affinity). SLIT supplies the relative distance matrix. Both are optional;
//! a machine without them behaves as a single locality.

use super::reader::Table;
use super::{MemoryDomain, Platform, MAX_MEMORY_DOMAINS};

const SRAT_ENTRY_START: usize = 36 + 4 + 8;

const TYPE_MEMORY: u8 = 1;

const MEMORY_ENABLED: u32 = 1 << 0;
const MEMORY_HOT_PLUGGABLE: u32 = 1 << 1;
const MEMORY_NON_VOLATILE: u32 = 1 << 2;

pub fn parse(table: &Table<'_>, platform: &mut Platform) {
    platform.has_srat = true;

    let mut offset = SRAT_ENTRY_START;
    let length = table.length();
    while offset + 2 <= length {
        let kind = table.u8(offset);
        let entry_length = table.u8(offset + 1) as usize;
        if entry_length < 2 || offset.saturating_add(entry_length) > length {
            break;
        }

        if kind == TYPE_MEMORY && entry_length >= 40 && platform.memory_domain_count < MAX_MEMORY_DOMAINS
        {
            let flags = table.u32(offset + 32);
            platform.memory_domains[platform.memory_domain_count] = MemoryDomain {
                domain: table.u32(offset + 2),
                base: table.u64(offset + 8),
                length: table.u64(offset + 16),
                enabled: flags & MEMORY_ENABLED != 0,
                hot_pluggable: flags & MEMORY_HOT_PLUGGABLE != 0,
                non_volatile: flags & MEMORY_NON_VOLATILE != 0,
            };
            platform.memory_domain_count += 1;
        }

        offset += entry_length;
    }
}

pub fn parse_slit(table: &Table<'_>, platform: &mut Platform) {
    platform.has_slit = true;

    let localities = table.u64(36);
    platform.locality_count = localities.min(u32::MAX as u64) as u32;

    let rows = localities.min(8) as usize;
    for row in 0..rows {
        // The matrix rows are `localities` bytes wide, independent of how
        // many rows are kept in the fixed-size summary.
        let row_start = 44usize.saturating_add(row.saturating_mul(localities as usize));
        for column in 0..rows {
            platform.locality_distance[row][column] = table.u8(row_start + column);
        }
    }
}
