//! Kernel physical page allocator wrapper.
//!
//! The allocator itself lives in `sketchos-mm`; this module supplies the UEFI
//! memory map, the fixed bitmap storage and the interrupt-safe lock.
//!
//! Only `CONVENTIONAL` memory is managed. Kernel, loader, boot-info and memory
//! map pages are `LOADER_*` in the final map, but they are reserved explicitly
//! as well so a firmware that classifies them differently cannot hand them to a
//! driver. Per-core stacks, page tables and the allocator bitmaps are inside the
//! kernel image and therefore covered by the kernel range.

use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use sketchos_bootinfo::{memory_kind, phys_to_virt, BootInfo, MemoryDescriptor};
use sketchos_mm::physical::{
    Error, Fragmentation, MemoryRange, PhysicalAllocator, Stats,
};

use crate::sync::IrqSpinLock;

/// Bitmap words for a 32 GiB managed span. 32 GiB / 4 KiB / 64 = 131,072
/// words, or 1 MiB per bitmap. The target laptop has 16 GiB of RAM; the extra
/// headroom covers a firmware map whose usable range is slightly larger.
pub const WORDS: usize = 131_072;

/// Maximum usable descriptors kept from the UEFI map.
const MAX_RANGES: usize = 128;

static ALLOCATOR: IrqSpinLock<PhysicalAllocator<WORDS>> =
    IrqSpinLock::new(PhysicalAllocator::empty());
static READY: AtomicBool = AtomicBool::new(false);

/// Initialize the allocator from the final UEFI map and reserve every range the
/// kernel must not hand out.
pub fn init(info: &BootInfo, boot_info_addr: u64) -> Result<(), Error> {
    let mut ranges = [MemoryRange::new(0, 0); MAX_RANGES];
    let mut count = 0;

    let descriptors = phys_to_virt(info.memory_map.address) as *const MemoryDescriptor;
    for index in 0..info.memory_map.count as usize {
        // SAFETY: the loader validated the descriptor array before handoff and
        // the map lives in loader-data pages reachable through the physmap.
        let entry = unsafe { ptr::read_volatile(descriptors.add(index)) };
        if entry.kind != memory_kind::CONVENTIONAL {
            continue;
        }
        let Some(end) = entry.end() else {
            continue;
        };
        if entry.physical_start >= end {
            continue;
        }
        if count == MAX_RANGES {
            return Err(Error::TooManyRanges);
        }
        ranges[count] = MemoryRange::new(entry.physical_start, end);
        count += 1;
    }

    let mut allocator = ALLOCATOR.lock();
    allocator.init(&ranges[..count])?;

    reserve(&mut allocator, 0, 4096)?;
    reserve(
        &mut allocator,
        info.kernel_phys_start,
        info.kernel_phys_end,
    )?;
    reserve(
        &mut allocator,
        boot_info_addr,
        boot_info_addr.saturating_add(size_of::<BootInfo>() as u64),
    )?;

    let map_bytes = info
        .memory_map
        .count
        .saturating_mul(info.memory_map.entry_size as u64);
    reserve(
        &mut allocator,
        info.memory_map.address,
        info.memory_map.address.saturating_add(map_bytes),
    )?;

    if info.loader_phys_start != 0 && info.loader_phys_end > info.loader_phys_start {
        reserve(
            &mut allocator,
            info.loader_phys_start,
            info.loader_phys_end,
        )?;
    }
    if info.rsdp_address != 0 {
        reserve(
            &mut allocator,
            info.rsdp_address,
            info.rsdp_address.saturating_add(4096),
        )?;
    }
    if info.has_flag(sketchos_bootinfo::FLAG_FRAMEBUFFER)
        && info.framebuffer.address != 0
        && info.framebuffer.size != 0
    {
        reserve(
            &mut allocator,
            info.framebuffer.address,
            info.framebuffer
                .address
                .saturating_add(info.framebuffer.size),
        )?;
    }
    if info.has_flag(sketchos_bootinfo::FLAG_UART) && info.uart_base != 0 {
        reserve(
            &mut allocator,
            info.uart_base,
            info.uart_base.saturating_add(4096),
        )?;
    }

    READY.store(true, Ordering::Release);
    Ok(())
}

fn reserve(
    allocator: &mut PhysicalAllocator<WORDS>,
    start: u64,
    end: u64,
) -> Result<(), Error> {
    if end <= start {
        return Ok(());
    }
    allocator.reserve_range(start, end)
}

/// Allocate `count` contiguous pages aligned to `align` bytes.
pub fn alloc_pages(count: usize, align: u64) -> Option<u64> {
    ALLOCATOR.lock().alloc_aligned(count, align)
}

/// Free a range previously returned by [`alloc_pages`].
///
/// The pages are zeroed after the allocator accepts the free and before the
/// lock is released, so the next owner can never observe the previous
/// contents. A rejected free is not scrubbed, because the range may not belong
/// to the caller.
pub fn free_pages(address: u64, count: usize) -> Result<(), Error> {
    let bytes = count
        .checked_mul(4096)
        .ok_or(Error::InvalidAddress)?;
    let mut allocator = ALLOCATOR.lock();
    allocator.free_pages(address, count)?;
    if bytes != 0 {
        // SAFETY: `free_pages` succeeded, so [address, address + bytes) is a
        // managed range this core owned and just released. The allocator lock
        // is held, so no other core can hand the pages out until the scrub is
        // complete. All managed pages are Normal memory in the physmap.
        unsafe { ptr::write_bytes(phys_to_virt(address) as *mut u8, 0, bytes) };
    }
    Ok(())
}

pub fn stats() -> Stats {
    ALLOCATOR.lock().stats()
}

pub fn fragmentation() -> Fragmentation {
    ALLOCATOR.lock().fragmentation()
}

pub fn is_managed(address: u64) -> bool {
    ALLOCATOR.lock().is_managed(address)
}

// Query helper kept for callers that check allocator readiness.
#[allow(dead_code)]
pub fn ready() -> bool {
    READY.load(Ordering::Acquire)
}
