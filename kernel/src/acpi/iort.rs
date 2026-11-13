//! IORT (I/O Remapping Table) decoding.
//!
//! IORT describes SMMUv3, ITS groups, root complexes and named components.
//! This milestone only validates the node array and counts nodes, because
//! programming an SMMU requires page tables and a DMA ownership model that
//! arrive in a later milestone.

use super::reader::Table;
use super::Platform;

const HEADER: usize = 36;
const NODE_ARRAY_OFFSET: usize = 40;

pub fn parse(table: &Table<'_>, platform: &mut Platform) {
    let mut offset = table.u32(NODE_ARRAY_OFFSET) as usize;
    if offset == 0 {
        offset = HEADER + 4 + 4 + 4;
    }
    let length = table.length();

    while offset + 4 <= length {
        let node_length = table.u16(offset + 1) as usize;
        if node_length < 4 || offset.saturating_add(node_length) > length {
            break;
        }
        platform.iort_nodes += 1;
        offset += node_length;
    }
}
