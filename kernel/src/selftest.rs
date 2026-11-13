//! Boot-time memory self-tests and measurements.
//!
//! The host tests in `sketchos-mm` cover the allocator algorithms in
//! isolation. These tests exercise the real kernel wrappers, the real UEFI
//! memory map and real page mappings, and record the numbers the milestone
//! report needs. A failure is fatal: the scheduler must never run on memory
//! the kernel cannot account for.
//!
//! Nothing here allocates from an interrupt handler, and every allocation is
//! released before the function returns.

use core::ptr;

use sketchos_bootinfo::{memory_kind, phys_to_virt, BootInfo};
use sketchos_mm::heap::HeapError;
use sketchos_mm::physical::Error as PhysError;

use crate::arch::aarch64::{counter, counter_frequency};
use crate::{heap, phys, println};

const PAGE: usize = 4096;

/// Exercise the physical allocator and print its measurements.
pub fn physical(info: &BootInfo, boot_info_addr: u64) -> Result<(), &'static str> {
    let stats = phys::stats();
    if stats.managed_pages == 0 {
        return Err("physical allocator manages no pages");
    }

    println!();
    println!(
        "Physical memory: managed {} MiB ({} pages), free {} MiB, span 0x{:016x}-0x{:016x}",
        mib(stats.managed_pages * 4096),
        stats.managed_pages,
        mib(stats.free_pages * 4096),
        stats.base_address,
        stats.end_address
    );

    // Basic allocation, data integrity and free.
    let page = phys::alloc_pages(1, PAGE as u64).ok_or("physical allocation failed")?;
    if page % PAGE as u64 != 0 {
        return Err("physical page is not 4 KiB aligned");
    }
    // SAFETY: the allocator returned an owned page, reachable through the
    // linear physical map.
    unsafe {
        let base = phys_to_virt(page) as *mut u8;
        ptr::write_bytes(base, 0x5a, PAGE);
        for offset in [0usize, PAGE / 2, PAGE - 1] {
            if ptr::read_volatile(base.add(offset)) != 0x5a {
                return Err("physical page data integrity check failed");
            }
        }
    }
    phys::free_pages(page, 1).map_err(|_| "physical free failed")?;

    // Invalid free and double free must be distinguished.
    if phys::free_pages(0, 1) != Err(PhysError::InvalidAddress) {
        return Err("physical allocator accepted an invalid free");
    }
    let page = phys::alloc_pages(1, PAGE as u64).ok_or("physical allocation failed")?;
    phys::free_pages(page, 1).map_err(|_| "physical free failed")?;
    if phys::free_pages(page, 1) != Err(PhysError::DoubleFree) {
        return Err("physical allocator did not detect a double free");
    }

    // 64 KiB alignment over 16 contiguous pages.
    let aligned = phys::alloc_pages(16, 64 * 1024).ok_or("aligned physical allocation failed")?;
    if aligned % (64 * 1024) != 0 {
        return Err("physical allocator ignored the requested alignment");
    }
    // SAFETY: 16 owned pages, reachable through the physical map.
    unsafe {
        let base = phys_to_virt(aligned) as *mut u8;
        ptr::write_bytes(base, 0xa5, 16 * PAGE);
        if ptr::read_volatile(base.add(16 * PAGE - 1)) != 0xa5 {
            return Err("aligned physical allocation data check failed");
        }
    }
    phys::free_pages(aligned, 16).map_err(|_| "aligned physical free failed")?;

    // Latency: 1000 single-page allocate/free pairs.
    let frequency = counter_frequency();
    let started = counter();
    for _ in 0..1000 {
        let page = phys::alloc_pages(1, PAGE as u64).ok_or("physical allocator exhausted")?;
        phys::free_pages(page, 1).map_err(|_| "physical free failed")?;
    }
    let elapsed = counter().wrapping_sub(started);
    println!(
        "Physical latency: 1000 alloc/free pairs in {} us ({} ns/pair)",
        micros(elapsed, frequency),
        nanos_per_pair(elapsed, frequency, 1000)
    );

    // Fragmentation: allocate 64 pages, free every other one, then all.
    let mut pages = [0u64; 64];
    let mut count = 0usize;
    for slot in pages.iter_mut() {
        match phys::alloc_pages(1, PAGE as u64) {
            Some(page) => {
                *slot = page;
                count += 1;
            }
            None => break,
        }
    }
    let before = phys::fragmentation();
    for index in (0..count).step_by(2) {
        phys::free_pages(pages[index], 1).map_err(|_| "physical free failed")?;
    }
    let alternating = phys::fragmentation();
    for index in (1..count).step_by(2) {
        phys::free_pages(pages[index], 1).map_err(|_| "physical free failed")?;
    }
    let after = phys::fragmentation();
    println!(
        "Physical fragmentation: {} free runs / largest {} pages before, {} / {} after alternating free, {} / {} after coalescing",
        before.free_runs,
        before.largest_free_run,
        alternating.free_runs,
        alternating.largest_free_run,
        after.free_runs,
        after.largest_free_run
    );

    let final_stats = phys::stats();
    if final_stats.allocated_pages != stats.allocated_pages {
        return Err("physical allocator leaked pages during self-test");
    }
    // The kernel image, boot-info record, memory map and every non-conventional
    // firmware descriptor must be outside the managed set. Checking both ends
    // of each range catches an off-by-one in the reserve path.
    if info.kernel_phys_end > info.kernel_phys_start {
        if phys::is_managed(info.kernel_phys_start)
            || phys::is_managed(info.kernel_phys_end - 1)
        {
            return Err("physical allocator would hand out the kernel image");
        }
    }
    if phys::is_managed(boot_info_addr) {
        return Err("physical allocator would hand out the boot-info record");
    }
    if phys::is_managed(info.memory_map.address) {
        return Err("physical allocator would hand out the memory map");
    }
    let descriptors = phys_to_virt(info.memory_map.address) as *const sketchos_bootinfo::MemoryDescriptor;
    for index in 0..info.memory_map.count as usize {
        // SAFETY: the loader validated the descriptor array before handoff.
        let entry = unsafe { ptr::read_volatile(descriptors.add(index)) };
        if entry.kind == memory_kind::CONVENTIONAL {
            continue;
        }
        let Some(end) = entry.end() else {
            continue;
        };
        if entry.physical_start >= end {
            continue;
        }
        if phys::is_managed(entry.physical_start) || phys::is_managed(end - 1) {
            return Err("physical allocator would hand out a reserved firmware range");
        }
    }

    // The free path must clear a page before returning it to the allocator.
    // Interrupts are still masked and no secondary is running, so nothing can
    // claim the page between the free and this read-back.
    let scrub_page = phys::alloc_pages(1, PAGE as u64).ok_or("physical allocation failed")?;
    // SAFETY: `scrub_page` is an owned page reachable through the physmap.
    unsafe {
        ptr::write_bytes(phys_to_virt(scrub_page) as *mut u8, 0xa5, PAGE);
    }
    phys::free_pages(scrub_page, 1).map_err(|_| "physical free failed")?;
    // SAFETY: the page was just freed but remains mapped in the physmap; the
    // boot self-test is the only allocator user at this point.
    unsafe {
        let base = phys_to_virt(scrub_page) as *const u8;
        for offset in [0usize, PAGE / 2, PAGE - 1] {
            if ptr::read_volatile(base.add(offset)) != 0 {
                return Err("physical free did not scrub the page");
            }
        }
    }

    println!("Physical self-test: PASS (alloc, free, alignment, invalid-free, double-free, scrub, reservations)");
    Ok(())
}

/// Exercise the kernel heap and print its measurements.
pub fn heap_test() -> Result<(), &'static str> {
    let initial = heap::stats();
    if initial.total_bytes == 0 {
        return Err("kernel heap has no regions");
    }

    // Small allocations with different alignments.
    for &(size, align) in &[(8usize, 8usize), (64, 16), (4096, 4096)] {
        let block = heap::alloc(size, align).ok_or("heap allocation failed")?;
        if block as usize % align != 0 {
            return Err("heap allocation ignored the requested alignment");
        }
        // SAFETY: the heap returned an owned block of `size` bytes.
        unsafe {
            ptr::write_bytes(block, 0x3c, size);
            if ptr::read_volatile(block.add(size - 1)) != 0x3c {
                return Err("heap data integrity check failed");
            }
        }
        heap::dealloc(block).map_err(|_| "heap free failed")?;
    }

    // A large block.
    let large = heap::alloc(48 * 1024, 8).ok_or("large heap allocation failed")?;
    // SAFETY: owned 48 KiB block.
    unsafe {
        ptr::write_bytes(large, 0x7e, 48 * 1024);
        if ptr::read_volatile(large.add(48 * 1024 - 1)) != 0x7e {
            return Err("large heap allocation data check failed");
        }
    }
    heap::dealloc(large).map_err(|_| "large heap free failed")?;

    // Repeated small allocate/free must not leak or fragment permanently.
    for index in 0..1000usize {
        let size = (index % 97) + 1;
        let align = 1usize << (index % 8);
        let block = heap::alloc(size, align).ok_or("repeated heap allocation failed")?;
        // SAFETY: owned block.
        unsafe {
            ptr::write_bytes(block, index as u8, size);
            if ptr::read_volatile(block.add(size - 1)) != index as u8 {
                return Err("repeated heap allocation data check failed");
            }
        }
        heap::dealloc(block).map_err(|_| "repeated heap free failed")?;
    }

    // Force growth with a request the remaining initial region cannot satisfy.
    let first = heap::alloc(200 * 1024, 8).ok_or("heap growth test allocation failed")?;
    let second = heap::alloc(100 * 1024, 8).ok_or("heap growth did not satisfy the request")?;
    heap::dealloc(first).map_err(|_| "heap growth free failed")?;
    heap::dealloc(second).map_err(|_| "heap growth free failed")?;

    // A request above the cap must fail without reserving more physical memory.
    let before_cap = heap::stats();
    if heap::alloc(heap::MAX_HEAP_BYTES, 8).is_some() {
        return Err("heap grew beyond its configured cap");
    }
    let after_cap = heap::stats();
    if after_cap.total_bytes != before_cap.total_bytes {
        return Err("heap grew for a request above its cap");
    }

    // Invalid and double free.
    let block = heap::alloc(128, 8).ok_or("heap allocation failed")?;
    if heap::dealloc(unsafe { block.add(1) }) != Err(HeapError::InvalidPointer) {
        return Err("heap accepted an invalid free");
    }
    heap::dealloc(block).map_err(|_| "heap free failed")?;
    if heap::dealloc(block) != Err(HeapError::DoubleFree) {
        return Err("heap did not detect a double free");
    }

    // Latency: 1000 64-byte allocate/free pairs.
    let frequency = counter_frequency();
    let started = counter();
    for _ in 0..1000 {
        let block = heap::alloc(64, 8).ok_or("heap latency allocation failed")?;
        heap::dealloc(block).map_err(|_| "heap latency free failed")?;
    }
    let elapsed = counter().wrapping_sub(started);
    println!(
        "Heap latency: 1000 64-byte alloc/free pairs in {} us ({} ns/pair)",
        micros(elapsed, frequency),
        nanos_per_pair(elapsed, frequency, 1000)
    );

    let stats = heap::stats();
    let fragmentation = heap::fragmentation();
    println!(
        "Heap: {} KiB from {} region additions, {} KiB free in {} blocks, largest {} KiB, waste {} KiB",
        stats.total_bytes / 1024,
        stats.region_additions,
        stats.free_bytes / 1024,
        stats.free_blocks,
        stats.largest_free_block / 1024,
        fragmentation.waste_bytes / 1024
    );
    println!(
        "Heap counters: {} allocations, {} frees, {} out-of-memory events",
        stats.alloc_count, stats.dealloc_count, stats.oom_count
    );

    if stats.allocated_bytes != 0 {
        return Err("heap leaked bytes during self-test");
    }
    // Every region was released, so the whole heap must be free. Adjacent
    // regions coalesce, so the number of free blocks can be smaller than the
    // number of region additions; it can never be larger.
    if stats.free_bytes != stats.total_bytes {
        return Err("heap did not return every byte to the free list");
    }
    if stats.free_blocks > stats.region_additions {
        return Err("heap has more free blocks than regions");
    }

    println!("Heap self-test: PASS (small, large, aligned, repeated, growth, growth cap, invalid-free, double-free)");
    Ok(())
}

fn micros(ticks: u64, frequency: u64) -> u64 {
    if frequency == 0 {
        return 0;
    }
    (ticks as u128 * 1_000_000 / frequency as u128) as u64
}

fn nanos_per_pair(ticks: u64, frequency: u64, pairs: u64) -> u64 {
    if frequency == 0 || pairs == 0 {
        return 0;
    }
    (ticks as u128 * 1_000_000_000 / frequency as u128 / pairs as u128) as u64
}

fn mib(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}
