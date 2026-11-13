//! PPTT (Processor Properties Topology Table) decoding.
//!
//! ACPI represents the processor hierarchy as variable-length nodes linked by
//! table-relative parent offsets. QEMU and firmware build containers for
//! sockets and clusters and put leaf processor nodes below them. This decoder
//! keeps the node graph, resolves each leaf's cluster ancestor, and maps the
//! ACPI processor id (the MADT UID) to a dense cluster index.
//!
//! A leaf directly below a physical package has no separate cluster container;
//! its package id is used as the cluster id. A system with neither package nor
//! cluster containers reports one cluster.

use super::reader::Table;
use super::{Platform, PpttNode, MAX_CPUS, MAX_PPTT_NODES, PPTT_CLUSTER_UNKNOWN};

const TYPE_PROCESSOR: u8 = 0;
const TYPE_CACHE: u8 = 1;
const TYPE_ID: u8 = 2;

const FLAG_PHYSICAL_PACKAGE: u8 = 1 << 0;
const FLAG_ACPI_ID_VALID: u8 = 1 << 1;
const FLAG_LEAF: u8 = 1 << 3;

pub fn parse(table: &Table<'_>, platform: &mut Platform) {
    let mut offset = 36usize;
    let length = table.length();

    while offset + 4 <= length {
        let kind = table.u8(offset);
        let node_length = table.u8(offset + 1) as usize;
        if node_length < 4 || offset.saturating_add(node_length) > length {
            break;
        }

        let flags = if node_length >= 5 {
            table.u8(offset + 4)
        } else {
            0
        };
        // The ACPI Processor Hierarchy Node places `Flags` at +4 (u32),
        // `Parent` at +8 (u32) and `ACPI Processor ID` at +12 (u32). Reading
        // the parent at +5 shifts it by three bytes and turns a 36-byte offset
        // into 0x24000000.
        let parent = if node_length >= 12 {
            table.u32(offset + 8)
        } else {
            0
        };
        let acpi_id = if node_length >= 16 && flags & FLAG_ACPI_ID_VALID != 0 {
            table.u32(offset + 12)
        } else {
            0
        };

        match kind {
            TYPE_PROCESSOR | TYPE_CACHE | TYPE_ID => {}
            _ => {}
        }
        platform.pptt_nodes += 1;
        if kind == TYPE_CACHE {
            platform.pptt_caches += 1;
        }
        if platform.pptt_node_count < MAX_PPTT_NODES {
            platform.pptt_node_info[platform.pptt_node_count] = PpttNode {
                offset: offset as u32,
                kind,
                flags,
                parent,
                acpi_id,
            };
            platform.pptt_node_count += 1;
        }

        offset += node_length;
    }

    derive_clusters(platform);
}

fn derive_clusters(platform: &mut Platform) {
    let mut cluster_ids = [0u32; MAX_CPUS];
    let mut cluster_count = 0usize;

    for index in 0..platform.pptt_node_count {
        let node = platform.pptt_node_info[index];
        if node.kind != TYPE_PROCESSOR
            || node.flags & FLAG_LEAF == 0
            || node.flags & FLAG_ACPI_ID_VALID == 0
        {
            continue;
        }
        let uid = node.acpi_id as usize;
        if uid >= MAX_CPUS {
            continue;
        }

        let raw = cluster_ancestor(platform, index);
        let mut dense = None;
        for (slot, id) in cluster_ids[..cluster_count].iter().enumerate() {
            if *id == raw {
                dense = Some(slot);
                break;
            }
        }
        let dense = match dense {
            Some(slot) => slot,
            None => {
                if cluster_count >= MAX_CPUS {
                    0
                } else {
                    cluster_ids[cluster_count] = raw;
                    cluster_count += 1;
                    cluster_count - 1
                }
            }
        };
        platform.pptt_processor_cluster[uid] = dense as u16;
    }

    platform.pptt_clusters = cluster_count;
}

/// Walk from a leaf towards the root until the node just below a physical
/// package is found. That node is the cluster container.
fn cluster_ancestor(platform: &Platform, leaf: usize) -> u32 {
    let mut current = leaf;
    for _ in 0..MAX_PPTT_NODES {
        let node = platform.pptt_node_info[current];
        if node.parent == 0 {
            return 0;
        }
        let Some(parent) = find_by_offset(platform, node.parent) else {
            return 0;
        };
        let parent_node = platform.pptt_node_info[parent];
        if parent_node.flags & FLAG_PHYSICAL_PACKAGE != 0 {
            if current == leaf {
                // No cluster container: the physical package is the cluster.
                // The table offset is a stable identity even when the package
                // node has no valid ACPI processor id.
                return parent_node.offset;
            }
            // The node just below the package is the cluster container. Use
            // its table offset, not an ACPI id: firmware is not required to
            // give a cluster container an ACPI processor id.
            return node.offset;
        }
        current = parent;
    }
    0
}

fn find_by_offset(platform: &Platform, offset: u32) -> Option<usize> {
    platform.pptt_node_info[..platform.pptt_node_count]
        .iter()
        .position(|node| node.offset == offset)
}

/// Cluster index for an ACPI processor UID, or [`PPTT_CLUSTER_UNKNOWN`].
pub fn processor_cluster(platform: &Platform, uid: u32) -> u16 {
    platform
        .pptt_processor_cluster
        .get(uid as usize)
        .copied()
        .unwrap_or(PPTT_CLUSTER_UNKNOWN)
}
