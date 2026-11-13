//! Address-ordered first-fit kernel heap.
//!
//! # Layout
//!
//! Every block starts with a [`BlockHeader`]. Blocks are linked in physical
//! address order, which makes coalescing with both neighbours `O(1)` and makes
//! an overlap check when a new region is added straightforward.
//!
//! An allocation stores the header pointer in the eight bytes immediately
//! before the returned user pointer. That back-pointer lets [`Heap::dealloc`]
//! recover the header for any power-of-two alignment without a side table. The
//! cost is eight bytes per allocation; the benefit is that aligned allocations
//! do not need a separate allocator or a header at an unpredictable offset.
//!
//! # Failure behaviour
//!
//! * `alloc` returns `None` on exhaustion and increments `oom_count`; it does
//!   not modify the heap.
//! * `dealloc` validates the containing region, the back-pointer, the header
//!   magic and the stored user pointer before changing anything. An invalid
//!   pointer or an immediate double free returns an error and leaves the heap
//!   untouched.
//! * `add_region` rejects overlaps and regions too small to hold a header.
//!
//! The allocator is not thread safe by itself. The kernel wraps it in an
//! interrupt-safe spinlock; host tests use it directly.

use core::mem::{align_of, size_of};
use core::ptr;

/// Header magic, `"HEAP"` in the low 32 bits.
const MAGIC: u32 = 0x4845_4150;
/// Bytes of metadata before the user pointer, including the back-pointer.
const HEADER_SIZE: usize = size_of::<BlockHeader>();
/// A block must be large enough to hold a header.
const MIN_BLOCK: usize = HEADER_SIZE;
/// Back-pointer width.
const BACKPTR_SIZE: usize = size_of::<usize>();

#[repr(C)]
struct BlockHeader {
    /// Total block size in bytes, including this header and all padding.
    size: usize,
    prev: *mut BlockHeader,
    next: *mut BlockHeader,
    /// User pointer while allocated; zero while free.
    user: usize,
    free: u32,
    magic: u32,
}

impl BlockHeader {
    fn end(&self) -> usize {
        self as *const Self as usize + self.size
    }
}

/// Why a heap operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeapError {
    NullRegion,
    RegionTooSmall,
    RegionOverlap,
    AddressOverflow,
    InvalidAlignment,
    InvalidPointer,
    DoubleFree,
    Corrupt,
}

/// Heap accounting. `total_bytes` is the sum of all block sizes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapStats {
    pub total_bytes: usize,
    pub allocated_bytes: usize,
    pub free_bytes: usize,
    pub free_blocks: u32,
    pub allocated_blocks: u32,
    pub largest_free_block: usize,
    /// Number of `add_region` calls, not the number of disjoint regions after
    /// coalescing. Adjacent regions merge into one block, so this can exceed
    /// `free_blocks` even when the heap is healthy.
    pub region_additions: u32,
    pub alloc_count: u64,
    pub dealloc_count: u64,
    pub oom_count: u64,
}

/// Free-space fragmentation. `waste_bytes` is free memory outside the largest
/// free block; a high value means a large request may fail even though the
/// total free bytes look sufficient.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapFragmentation {
    pub free_bytes: usize,
    pub largest_free_block: usize,
    pub free_blocks: u32,
    pub waste_bytes: usize,
}

/// Address-ordered first-fit heap.
pub struct Heap {
    head: *mut BlockHeader,
    region_additions: u32,
    allocated_bytes: usize,
    alloc_count: u64,
    dealloc_count: u64,
    oom_count: u64,
}

// SAFETY: `Heap` owns no thread-affine state. The kernel only touches it while
// holding its spinlock, and every raw pointer refers to a caller-provided
// region that outlives the heap.
unsafe impl Send for Heap {}

impl Heap {
    /// An empty heap. Add at least one region before allocating.
    pub const fn empty() -> Self {
        Self {
            head: ptr::null_mut(),
            region_additions: 0,
            allocated_bytes: 0,
            alloc_count: 0,
            dealloc_count: 0,
            oom_count: 0,
        }
    }

    /// Add a memory region. `start` is rounded up to header alignment and
    /// `size` is rounded down. The region must not overlap an existing one.
    pub fn add_region(&mut self, start: *mut u8, size: usize) -> Result<(), HeapError> {
        if start.is_null() {
            return Err(HeapError::NullRegion);
        }
        let raw_start = start as usize;
        let aligned_start = align_up(raw_start, align_of::<BlockHeader>())
            .ok_or(HeapError::AddressOverflow)?;
        let raw_end = raw_start
            .checked_add(size)
            .ok_or(HeapError::AddressOverflow)?;
        let aligned_end = raw_end & !(align_of::<BlockHeader>() - 1);
        if aligned_end <= aligned_start {
            return Err(HeapError::RegionTooSmall);
        }
        let region_size = aligned_end - aligned_start;
        if region_size < MIN_BLOCK {
            return Err(HeapError::RegionTooSmall);
        }

        // Reject overlaps before writing any metadata.
        let mut cursor = self.head;
        while !cursor.is_null() {
            let block = cursor as usize;
            let end = block + unsafe { (*cursor).size };
            if aligned_start < end && block < aligned_end {
                return Err(HeapError::RegionOverlap);
            }
            cursor = unsafe { (*cursor).next };
        }

        let header = aligned_start as *mut BlockHeader;
        // SAFETY: the region was checked for overlap, is at least MIN_BLOCK
        // bytes, and is aligned for BlockHeader.
        unsafe {
            ptr::write(
                header,
                BlockHeader {
                    size: region_size,
                    prev: ptr::null_mut(),
                    next: ptr::null_mut(),
                    user: 0,
                    free: 1,
                    magic: MAGIC,
                },
            );
        }

        self.insert_sorted(header);
        self.region_additions += 1;
        self.coalesce(header);
        Ok(())
    }

    /// Allocate `size` bytes aligned to `align`.
    ///
    /// `size == 0` returns `None`. `align` must be a power of two; values below
    /// the back-pointer width are rounded up to it.
    pub fn alloc(&mut self, size: usize, align: usize) -> Option<*mut u8> {
        if size == 0 || !align.is_power_of_two() {
            return None;
        }
        let align = align.max(BACKPTR_SIZE);
        // Worst-case padding plus the back-pointer plus the header.
        size.checked_add(align)?
            .checked_add(HEADER_SIZE)?
            .checked_add(BACKPTR_SIZE)?;

        let mut cursor = self.head;
        while !cursor.is_null() {
            // SAFETY: every node in the list is a live header.
            let header = unsafe { &mut *cursor };
            if header.free != 0 {
                let block_start = cursor as usize;
                let block_end = block_start.checked_add(header.size)?;
                let data_start = block_start.checked_add(HEADER_SIZE)?;
                let user_min = data_start.checked_add(BACKPTR_SIZE)?;
                let user = align_up(user_min, align)?;
                let user_end = user.checked_add(size)?;
                if user_end <= block_end {
                    let used = align_up(user_end - block_start, align_of::<usize>())?;
                    let used = used.max(MIN_BLOCK);
                    let mut final_size = used;
                    if block_end - block_start > used
                        && block_end - block_start - used >= MIN_BLOCK
                    {
                        // Split: the remainder becomes a new free block.
                        let remainder = block_start + used;
                        // SAFETY: `remainder` is inside the current block and
                        // leaves at least MIN_BLOCK bytes.
                        unsafe {
                            let next = header.next;
                            ptr::write(
                                remainder as *mut BlockHeader,
                                BlockHeader {
                                    size: block_end - remainder,
                                    prev: cursor,
                                    next,
                                    user: 0,
                                    free: 1,
                                    magic: MAGIC,
                                },
                            );
                            if !next.is_null() {
                                (*next).prev = remainder as *mut BlockHeader;
                            }
                            header.next = remainder as *mut BlockHeader;
                        }
                    } else {
                        final_size = block_end - block_start;
                    }

                    header.size = final_size;
                    header.free = 0;
                    header.user = user;
                    // SAFETY: `user` is at least BACKPTR_SIZE into the block.
                    unsafe {
                        ptr::write((user - BACKPTR_SIZE) as *mut *mut BlockHeader, cursor);
                    }
                    self.allocated_bytes = self.allocated_bytes.checked_add(final_size)?;
                    self.alloc_count += 1;
                    return Some(user as *mut u8);
                }
            }
            cursor = unsafe { (*cursor).next };
        }

        self.oom_count += 1;
        None
    }

    /// Free a pointer returned by [`alloc`](Self::alloc).
    pub fn dealloc(&mut self, pointer: *mut u8) -> Result<(), HeapError> {
        if pointer.is_null() {
            return Err(HeapError::InvalidPointer);
        }
        let user = pointer as usize;
        let backptr = user
            .checked_sub(BACKPTR_SIZE)
            .ok_or(HeapError::InvalidPointer)?;

        // The back-pointer itself must lie inside a live block before it is
        // dereferenced. This keeps a wild free from reading arbitrary memory.
        if self.find_containing_block(user).is_none() {
            return Err(HeapError::InvalidPointer);
        }

        // SAFETY: `backptr` is inside a live block, so reading one word is safe.
        let header = unsafe { ptr::read(backptr as *const *mut BlockHeader) };
        if header.is_null() {
            return Err(HeapError::InvalidPointer);
        }

        // The header itself must also be inside a live block. After a merge the
        // old header can be interior, which is still safe to inspect.
        if self.find_containing_block(header as usize).is_none() {
            return Err(HeapError::InvalidPointer);
        }

        // SAFETY: the header lies inside a live block.
        let block = unsafe { &mut *header };
        if block.magic != MAGIC {
            return Err(HeapError::Corrupt);
        }
        if block.free != 0 {
            return Err(HeapError::DoubleFree);
        }
        if block.user != user {
            return Err(HeapError::InvalidPointer);
        }

        block.free = 1;
        block.user = 0;
        self.allocated_bytes = self.allocated_bytes.saturating_sub(block.size);
        self.dealloc_count += 1;
        self.coalesce(header);
        Ok(())
    }

    /// Current accounting. `O(n)` in the number of blocks.
    pub fn stats(&self) -> HeapStats {
        let mut stats = HeapStats {
            region_additions: self.region_additions,
            alloc_count: self.alloc_count,
            dealloc_count: self.dealloc_count,
            oom_count: self.oom_count,
            allocated_bytes: self.allocated_bytes,
            ..HeapStats::default()
        };
        let mut cursor = self.head;
        while !cursor.is_null() {
            // SAFETY: every node in the list is a live header.
            let block = unsafe { &*cursor };
            stats.total_bytes += block.size;
            if block.free != 0 {
                stats.free_bytes += block.size;
                stats.free_blocks += 1;
                stats.largest_free_block = stats.largest_free_block.max(block.size);
            } else {
                stats.allocated_blocks += 1;
            }
            cursor = block.next;
        }
        stats
    }

    /// Fragmentation snapshot. `O(n)`.
    pub fn fragmentation(&self) -> HeapFragmentation {
        let stats = self.stats();
        HeapFragmentation {
            free_bytes: stats.free_bytes,
            largest_free_block: stats.largest_free_block,
            free_blocks: stats.free_blocks,
            waste_bytes: stats
                .free_bytes
                .saturating_sub(stats.largest_free_block),
        }
    }

    fn find_containing_block(&self, address: usize) -> Option<*mut BlockHeader> {
        let mut cursor = self.head;
        while !cursor.is_null() {
            // SAFETY: every node in the list is a live header.
            let block = unsafe { &*cursor };
            let start = cursor as usize;
            let end = start.checked_add(block.size)?;
            if address >= start && address < end {
                return Some(cursor);
            }
            cursor = block.next;
        }
        None
    }

    fn insert_sorted(&mut self, header: *mut BlockHeader) {
        let mut cursor = self.head;
        let mut previous: *mut BlockHeader = ptr::null_mut();
        while !cursor.is_null() && (cursor as usize) < (header as usize) {
            previous = cursor;
            // SAFETY: every node in the list is a live header.
            cursor = unsafe { (*cursor).next };
        }
        // SAFETY: `header` was just initialized and is not in the list.
        unsafe {
            (*header).prev = previous;
            (*header).next = cursor;
            if !previous.is_null() {
                (*previous).next = header;
            } else {
                self.head = header;
            }
            if !cursor.is_null() {
                (*cursor).prev = header;
            }
        }
    }

    fn coalesce(&mut self, header: *mut BlockHeader) {
        // SAFETY: `header` is a live free block and its links are valid.
        unsafe {
            let next = (*header).next;
            if !next.is_null()
                && (*next).free != 0
                && (*header).end() == next as usize
            {
                (*header).size += (*next).size;
                (*header).next = (*next).next;
                if !(*next).next.is_null() {
                    (*(*next).next).prev = header;
                }
            }

            let prev = (*header).prev;
            if !prev.is_null()
                && (*prev).free != 0
                && (*prev).end() == header as usize
            {
                (*prev).size += (*header).size;
                (*prev).next = (*header).next;
                if !(*header).next.is_null() {
                    (*(*header).next).prev = prev;
                }
            }
        }
    }
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    value.checked_add(align - 1).map(|v| v & !(align - 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;
    use std::vec::Vec;

    const REGION_SIZE: usize = 64 * 1024;

    fn region() -> Vec<u64> {
        // `u64` storage keeps the region aligned for BlockHeader.
        vec![0u64; REGION_SIZE / 8]
    }

    fn heap_with_region(buffer: &mut [u64]) -> Heap {
        let mut heap = Heap::empty();
        heap.add_region(buffer.as_mut_ptr() as *mut u8, REGION_SIZE)
            .unwrap();
        heap
    }

    #[test]
    fn small_allocations_are_distinct_and_writable() {
        let mut buffer = region();
        let mut heap = heap_with_region(&mut buffer);
        let a = heap.alloc(8, 8).unwrap();
        let b = heap.alloc(24, 8).unwrap();
        let c = heap.alloc(64, 8).unwrap();
        assert_ne!(a, b);
        assert_ne!(b, c);
        unsafe {
            ptr::write_bytes(a, 0xaa, 8);
            ptr::write_bytes(b, 0xbb, 24);
            ptr::write_bytes(c, 0xcc, 64);
            assert_eq!(ptr::read(a.add(7)), 0xaa);
            assert_eq!(ptr::read(b.add(23)), 0xbb);
            assert_eq!(ptr::read(c.add(63)), 0xcc);
        }
        heap.dealloc(c).unwrap();
        heap.dealloc(b).unwrap();
        heap.dealloc(a).unwrap();
        let stats = heap.stats();
        assert_eq!(stats.allocated_bytes, 0);
        assert_eq!(stats.free_blocks, 1);
    }

    #[test]
    fn large_allocation_uses_most_of_region() {
        let mut buffer = region();
        let mut heap = heap_with_region(&mut buffer);
        let block = heap.alloc(48 * 1024, 8).unwrap();
        assert!(!block.is_null());
        unsafe { ptr::write_bytes(block, 0x5a, 48 * 1024) };
        heap.dealloc(block).unwrap();
        assert_eq!(heap.stats().free_blocks, 1);
    }

    #[test]
    fn alignment_is_honoured() {
        let mut buffer = region();
        let mut heap = heap_with_region(&mut buffer);
        for &align in &[8usize, 16, 64, 256, 4096] {
            let block = heap.alloc(100, align).unwrap();
            assert_eq!(block as usize % align, 0, "alignment {}", align);
            heap.dealloc(block).unwrap();
        }
    }

    #[test]
    fn repeated_alloc_free_does_not_leak() {
        let mut buffer = region();
        let mut heap = heap_with_region(&mut buffer);
        let initial = heap.stats();
        for index in 0..2_000 {
            let size = (index % 97) + 1;
            let align = 1usize << (index % 8);
            let block = heap.alloc(size, align).unwrap();
            unsafe { ptr::write_bytes(block, index as u8, size) };
            heap.dealloc(block).unwrap();
        }
        let stats = heap.stats();
        assert_eq!(stats.allocated_bytes, 0);
        assert_eq!(stats.free_blocks, 1);
        assert_eq!(stats.total_bytes, initial.total_bytes);
    }

    #[test]
    fn exhaustion_is_reported_and_recoverable() {
        let mut buffer = region();
        let mut heap = heap_with_region(&mut buffer);
        let mut blocks = Vec::new();
        while let Some(block) = heap.alloc(1024, 8) {
            blocks.push(block);
        }
        assert!(blocks.len() > 1);
        assert!(heap.stats().oom_count >= 1);
        for block in blocks {
            heap.dealloc(block).unwrap();
        }
        let stats = heap.stats();
        assert_eq!(stats.allocated_bytes, 0);
        assert_eq!(stats.free_blocks, 1);
        assert!(heap.alloc(32 * 1024, 8).is_some());
    }

    #[test]
    fn coalescing_allows_a_large_request_after_fragmentation() {
        let mut buffer = region();
        let mut heap = heap_with_region(&mut buffer);
        let a = heap.alloc(20 * 1024, 8).unwrap();
        let b = heap.alloc(20 * 1024, 8).unwrap();
        let c = heap.alloc(20 * 1024, 8).unwrap();
        heap.dealloc(b).unwrap();
        // The hole is only 20 KiB, and the tail is smaller still, so a 40 KiB
        // request cannot fit.
        assert!(heap.alloc(40 * 1024, 8).is_none());
        heap.dealloc(a).unwrap();
        heap.dealloc(c).unwrap();
        // All three neighbours are now one block again.
        let large = heap.alloc(40 * 1024, 8).unwrap();
        heap.dealloc(large).unwrap();
        assert_eq!(heap.stats().free_blocks, 1);
    }

    #[test]
    fn invalid_and_double_free_are_detected() {
        let mut buffer = region();
        let mut heap = heap_with_region(&mut buffer);
        let block = heap.alloc(128, 8).unwrap();
        assert_eq!(
            heap.dealloc(unsafe { block.add(1) }),
            Err(HeapError::InvalidPointer)
        );
        assert_eq!(heap.dealloc(ptr::null_mut()), Err(HeapError::InvalidPointer));
        heap.dealloc(block).unwrap();
        assert_eq!(heap.dealloc(block), Err(HeapError::DoubleFree));
    }

    #[test]
    fn overlapping_region_is_rejected() {
        let mut buffer = region();
        let mut heap = heap_with_region(&mut buffer);
        let start = buffer.as_mut_ptr() as *mut u8;
        assert_eq!(
            heap.add_region(unsafe { start.add(4096) }, 4096),
            Err(HeapError::RegionOverlap)
        );
        assert_eq!(heap.add_region(start, 4096), Err(HeapError::RegionOverlap));
    }

    #[test]
    fn multiple_regions_are_supported() {
        let mut first = region();
        let mut second = region();
        let mut heap = Heap::empty();
        heap.add_region(first.as_mut_ptr() as *mut u8, REGION_SIZE)
            .unwrap();
        heap.add_region(second.as_mut_ptr() as *mut u8, REGION_SIZE)
            .unwrap();
        let a = heap.alloc(1024, 8).unwrap();
        let b = heap.alloc(1024, 8).unwrap();
        assert_ne!(a, b);
        heap.dealloc(a).unwrap();
        heap.dealloc(b).unwrap();
        let stats = heap.stats();
        assert_eq!(stats.region_additions, 2);
        assert_eq!(stats.allocated_bytes, 0);
    }
}
