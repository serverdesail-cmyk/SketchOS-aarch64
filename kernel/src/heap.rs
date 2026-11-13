//! Kernel heap wrapper around the `sketchos-mm` free-list allocator.
//!
//! The heap grows in whole 4 KiB pages from the physical allocator. Growth
//! happens with the heap lock released, then the new region is added under the
//! lock. The lock order is always heap -> physical; nothing acquires them in the
//! opposite order.
//!
//! No interrupt handler allocates. The scheduler and IPI paths only touch
//! preallocated per-core state, so an IRQ can never observe a half-grown heap.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use sketchos_mm::heap::{Heap, HeapError, HeapFragmentation, HeapStats};
use sketchos_bootinfo::phys_to_virt;

use crate::phys;
use crate::sync::IrqSpinLock;

/// Initial heap size: 256 KiB.
pub const INITIAL_PAGES: usize = 64;
/// Minimum growth chunk: 256 KiB. A larger request grows by enough pages to
/// satisfy that request instead of adding one fixed chunk and retrying.
pub const GROW_PAGES: usize = 64;
/// Hard cap on the heap. Growth never reserves more than this, so a single
/// oversized request cannot consume physical memory indefinitely.
pub const MAX_HEAP_BYTES: usize = 8 * 1024 * 1024;
/// Slack added to a growth request for the block header, the alignment
/// back-pointer and alignment rounding.
const GROWTH_OVERHEAD: usize = 64;

static HEAP: IrqSpinLock<Heap> = IrqSpinLock::new(Heap::empty());
static READY: AtomicBool = AtomicBool::new(false);
/// Bytes currently owned by the heap. Updated only while the heap lock is held.
static HEAP_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Allocate the initial region and make the heap usable.
pub fn init() -> Result<(), &'static str> {
    let base = phys::alloc_pages(INITIAL_PAGES, 4096)
        .ok_or("physical allocator has no contiguous pages for the heap")?;
    let mut heap = HEAP.lock();
    heap.add_region(phys_to_virt(base) as *mut u8, INITIAL_PAGES * 4096)
        .map_err(|_| "heap rejected its initial region")?;
    HEAP_BYTES.store(INITIAL_PAGES * 4096, Ordering::Release);
    READY.store(true, Ordering::Release);
    Ok(())
}

/// Allocate `size` bytes with `align`-byte alignment. Returns null on failure.
pub fn alloc(size: usize, align: usize) -> Option<*mut u8> {
    if !READY.load(Ordering::Acquire) {
        return None;
    }

    {
        let mut heap = HEAP.lock();
        if let Some(pointer) = heap.alloc(size, align) {
            return Some(pointer);
        }
    }

    // The existing free space could not satisfy the request. Size the next
    // region to the request itself: a fixed chunk would satisfy nothing larger
    // than a chunk and would keep being added on every retry.
    let slack = align.saturating_sub(1).saturating_add(GROWTH_OVERHEAD);
    let needed = size.checked_add(slack)?;
    // A request that cannot fit under the cap must fail before any physical
    // pages are taken. The existing free space was just proven insufficient,
    // so waiting until after the allocation would only reserve useless pages.
    let used = HEAP_BYTES.load(Ordering::Acquire);
    if used.saturating_add(needed) > MAX_HEAP_BYTES {
        return None;
    }
    let pages = needed.checked_add(4095)? / 4096;
    let pages = pages.max(GROW_PAGES);

    // Clamp the chunk to the remaining budget. A request that cannot fit under
    // the cap fails without allocating anything.
    let used = HEAP_BYTES.load(Ordering::Acquire);
    let remaining = MAX_HEAP_BYTES.saturating_sub(used);
    let pages = pages.min(remaining / 4096);
    if pages < GROW_PAGES {
        return None;
    }
    let bytes = pages * 4096;
    let base = phys::alloc_pages(pages, 4096)?;

    let mut heap = HEAP.lock();
    // Re-check the cap under the lock: another allocation may have grown the
    // heap while the physical pages were being taken.
    let used = HEAP_BYTES.load(Ordering::Acquire);
    if used.saturating_add(bytes) > MAX_HEAP_BYTES {
        drop(heap);
        let _ = phys::free_pages(base, pages);
        return None;
    }
    if heap.add_region(phys_to_virt(base) as *mut u8, bytes).is_err() {
        drop(heap);
        let _ = phys::free_pages(base, pages);
        return None;
    }
    HEAP_BYTES.store(used.saturating_add(bytes), Ordering::Release);
    heap.alloc(size, align)
}

/// Free a pointer returned by [`alloc`].
pub fn dealloc(pointer: *mut u8) -> Result<(), HeapError> {
    HEAP.lock().dealloc(pointer)
}

pub fn stats() -> HeapStats {
    HEAP.lock().stats()
}

pub fn fragmentation() -> HeapFragmentation {
    HEAP.lock().fragmentation()
}

// Query helper kept for callers that check heap readiness without allocating.
#[allow(dead_code)]
pub fn ready() -> bool {
    READY.load(Ordering::Acquire)
}
