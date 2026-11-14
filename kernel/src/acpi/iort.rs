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
    let length = table.length();
    let node_count = table.u32(HEADER) as usize;
    let mut offset = table.u32(NODE_ARRAY_OFFSET) as usize;
    if offset == 0 {
        offset = HEADER + 4 + 4 + 4;
    }
    if offset < HEADER + 12 || offset > length {
        return;
    }

    // The IORT node array is explicitly counted. Do not interpret trailing
    // mapping/padding bytes as extra nodes, and cap work by the table length.
    let max_nodes = (length - offset) / 16;
    let nodes_to_parse = node_count.min(max_nodes);
    for _ in 0..nodes_to_parse {
        if offset.checked_add(16).map_or(true, |end| end > length) {
            break;
        }
        let node_length = table.u16(offset + 1) as usize;
        let node_end = match offset.checked_add(node_length) {
            Some(end) if node_length >= 16 && end <= length => end,
            _ => break,
        };
        platform.iort_nodes += 1;
        offset = node_end;
    }
}
