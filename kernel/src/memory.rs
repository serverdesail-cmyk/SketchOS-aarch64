//! Memory inventory: firmware memory map totals plus SRAT/SLIT/PPTT.
//!
//! The UEFI map is the authority for usable RAM. ACPI only adds affinity and
//! cache information, which this milestone reports but does not act on.

use crate::acpi::Platform;
use crate::print;
use crate::println;
use sketchos_bootinfo::{memory_kind, phys_to_virt, BootInfo, MemoryDescriptor};

pub fn report(info: &BootInfo, platform: &Platform) {
    let (usable, highest_end, regions) = map_totals(info);

    println!();
    println!("Memory:");
    println!(
        "  UEFI map: {} descriptors, {} MiB usable, top 0x{:016x}",
        regions,
        usable / (1024 * 1024),
        highest_end
    );

    if platform.memory_domain_count == 0 {
        println!("  SRAT: no memory affinity ranges described");
    } else {
        println!("  SRAT memory domains:");
        let mut enabled_bytes = 0u64;
        for domain in platform.memory_domains[..platform.memory_domain_count].iter() {
            enabled_bytes = enabled_bytes.saturating_add(if domain.enabled {
                domain.length
            } else {
                0
            });
            println!(
                "    domain {}: 0x{:016x}-0x{:016x} {} MiB{}{}{}",
                domain.domain,
                domain.base,
                domain.base.saturating_add(domain.length),
                domain.length / (1024 * 1024),
                if domain.enabled { "" } else { " disabled" },
                if domain.hot_pluggable { " hot-pluggable" } else { "" },
                if domain.non_volatile { " non-volatile" } else { "" }
            );
        }
        println!(
            "  SRAT enabled memory: {} MiB across {} domains",
            enabled_bytes / (1024 * 1024),
            platform.memory_domain_count
        );
    }

    println!(
        "  SLIT: {} localities{}, PPTT: {} nodes / {} caches / {} clusters",
        platform.locality_count,
        if platform.has_slit { "" } else { " (absent)" },
        platform.pptt_nodes,
        platform.pptt_caches,
        platform.pptt_clusters
    );
    if platform.pptt_node_count != 0 {
        for (index, node) in platform.pptt_node_info[..platform.pptt_node_count]
            .iter()
            .enumerate()
        {
            println!(
                "    pptt {}: type {}, flags 0x{:02x}, parent {}, id {}",
                index, node.kind, node.flags, node.parent, node.acpi_id
            );
        }
    }
    if platform.locality_count != 0 {
        let rows = platform.locality_count.min(8) as usize;
        for row in 0..rows {
            print!("    distance {}:", row);
            for column in 0..rows {
                print!(" {:>3}", platform.locality_distance[row][column]);
            }
            println!();
        }
        if platform.locality_count > 8 {
            println!("    (first 8 of {} localities shown)", platform.locality_count);
        }
    }
}

fn map_totals(info: &BootInfo) -> (u64, u64, usize) {
    let region = info.memory_map;
    let count = region.count as usize;
    let base = phys_to_virt(region.address) as *const MemoryDescriptor;
    let mut usable = 0u64;
    let mut highest_end = 0u64;

    for index in 0..count {
        // SAFETY: the loader allocated `count` descriptors at this address and
        // validated the region size before the handoff.
        let entry = unsafe { core::ptr::read(base.add(index)) };
        let bytes = entry.size_bytes();
        if is_usable(entry.kind) {
            usable = usable.saturating_add(bytes);
        }
        highest_end = highest_end.max(entry.end().unwrap_or(entry.physical_start));
    }

    (usable, highest_end, count)
}

fn is_usable(kind: u32) -> bool {
    matches!(
        kind,
        memory_kind::CONVENTIONAL
            | memory_kind::LOADER_CODE
            | memory_kind::LOADER_DATA
            | memory_kind::BOOT_SERVICES_CODE
            | memory_kind::BOOT_SERVICES_DATA
            | memory_kind::ACPI_RECLAIM
    )
}
