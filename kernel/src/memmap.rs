//! Final UEFI memory-map diagnostics.
//!
//! The kernel allocates from this map through the physical page allocator;
//! 0.0.4 verifies that the handoff pages survived `ExitBootServices` and that
//! the kernel image and boot record are described as loader data.

use crate::println;
use sketchos_bootinfo::{memory_kind, phys_to_virt, BootInfo, MemoryDescriptor};

const MAX_REGION_LINES: usize = 32;

pub fn report(info: &BootInfo, boot_info_address: u64) {
    let region = info.memory_map;
    let count = region.count as usize;
    if count == 0 {
        println!("Memory map: EMPTY");
        return;
    }
    let base = phys_to_virt(region.address) as *const MemoryDescriptor;

    let mut totals = [0u64; 16];
    let mut highest_end = 0u64;

    let mut kernel_kind = None;
    let mut boot_info_kind = None;
    let mut map_start_kind = None;
    let mut map_end_kind = None;

    let last_descriptor = region.address + (count as u64 - 1) * region.entry_size as u64;

    for index in 0..count {
        // SAFETY: the loader allocated `count` descriptors at this address
        // and validated the size before the handoff.
        let entry = unsafe { core::ptr::read(base.add(index)) };

        let slot = if entry.kind <= memory_kind::PERSISTENT {
            entry.kind as usize
        } else {
            15
        };
        totals[slot] = totals[slot].saturating_add(entry.page_count);

        let end = entry.end().unwrap_or(entry.physical_start);
        highest_end = highest_end.max(end);

        if covers(&entry, info.kernel_phys_start) && covers(&entry, info.kernel_phys_end - 1) {
            kernel_kind.get_or_insert(entry.kind);
        }
        if covers(&entry, boot_info_address) {
            boot_info_kind.get_or_insert(entry.kind);
        }
        if covers(&entry, region.address) {
            map_start_kind.get_or_insert(entry.kind);
        }
        if covers(&entry, last_descriptor) {
            map_end_kind.get_or_insert(entry.kind);
        }
    }

    println!(
        "Memory map: {} entries, {} MiB described, top 0x{:016x}",
        count,
        highest_end / (1024 * 1024),
        highest_end
    );

    for slot in 0..16usize {
        if totals[slot] == 0 {
            continue;
        }
        let kind = if slot == 15 {
            memory_kind::UNKNOWN
        } else {
            slot as u32
        };
        println!(
            "  {:<22} {:>8} MiB",
            memory_kind::name(kind),
            mib(totals[slot])
        );
    }

    println!("Conventional regions of at least 1 MiB:");
    let mut shown = 0usize;
    for index in 0..count {
        // SAFETY: same bounds as above.
        let entry = unsafe { core::ptr::read(base.add(index)) };
        if entry.kind != memory_kind::CONVENTIONAL || entry.page_count < 256 {
            continue;
        }
        println!(
            "  0x{:016x}-0x{:016x}  {:>6} MiB",
            entry.physical_start,
            entry.end().unwrap_or(entry.physical_start),
            mib(entry.page_count)
        );
        shown += 1;
        if shown == MAX_REGION_LINES {
            println!("  ... further conventional regions not shown");
            break;
        }
    }
    if shown == 0 {
        println!("  (none)");
    }

    println!(
        "Handoff pages: kernel {}, boot info {}, map {}..{}",
        kind_name(kernel_kind),
        kind_name(boot_info_kind),
        kind_name(map_start_kind),
        kind_name(map_end_kind)
    );

    let expected = memory_kind::LOADER_DATA;
    let all_loader_data = kernel_kind == Some(expected)
        && boot_info_kind == Some(expected)
        && map_start_kind == Some(expected)
        && map_end_kind == Some(expected);
    if !all_loader_data {
        println!("WARNING: handoff pages are not all described as loader data");
    }
}

fn covers(entry: &MemoryDescriptor, address: u64) -> bool {
    let end = entry.end().unwrap_or(entry.physical_start);
    address >= entry.physical_start && address < end
}

fn kind_name(kind: Option<u32>) -> &'static str {
    kind.map(memory_kind::name).unwrap_or("not-found")
}

fn mib(pages: u64) -> u64 {
    pages.saturating_mul(4096) / (1024 * 1024)
}
