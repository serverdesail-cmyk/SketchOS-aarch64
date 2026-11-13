//! 4 KiB bitmap physical page allocator.
//!
//! # Design
//!
//! Two bitmaps cover a single page-number span:
//!
//! * `managed` marks pages the kernel may hand out.
//! * `allocated` marks pages currently owned by a caller.
//!
//! A page is free when it is managed and not allocated. Reserving a range
//! clears its managed bits, so a reserved page is never returned by an
//! allocation and freeing it is reported as an invalid free. Freeing a page
//! that is managed but not allocated is reported as a double free. This is the
//! distinction the mission asks for; a single free bitmap cannot make it.
//!
//! The span is `highest usable end - lowest usable start`, not the highest
//! physical address in the firmware map. That matters on the target laptop,
//! whose MMIO windows sit far above RAM: indexing by absolute physical address
//! would waste megabytes of bitmap for addresses that can never be allocated.
//!
//! The storage is a caller-supplied `[u64; WORDS]`, so the kernel can place it
//! in BSS and host tests can use a small const parameter. The allocator never
//! allocates metadata and never overflows an address: every range is rounded
//! and checked with `checked_*` arithmetic.

/// Physical page size. The mission requires 4 KiB pages.
pub const PAGE_SIZE: u64 = 4096;

/// A usable physical range, end-exclusive and in bytes.
///
/// The kernel passes only ranges it is allowed to allocate from (UEFI
/// `CONVENTIONAL` memory). Everything else is simply absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryRange {
    pub start: u64,
    pub end: u64,
}

impl MemoryRange {
    pub const fn new(start: u64, end: u64) -> Self {
        Self { start, end }
    }
}

/// Why a bitmap allocator operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The caller supplied no non-empty usable range.
    NoMemory,
    /// The supplied bitmap storage cannot cover the requested span.
    StorageTooSmall {
        required_words: usize,
        available_words: usize,
    },
    /// A range or page index overflowed 64-bit arithmetic.
    AddressOverflow,
    /// The address is not page aligned.
    Misaligned,
    /// The address is outside the managed span or not managed.
    InvalidAddress,
    /// The page is managed but was already free.
    DoubleFree,
    /// A reservation tried to take a page that is currently allocated.
    ReserveAllocated,
    /// The caller supplied more usable ranges than the kernel's fixed array.
    TooManyRanges,
}

/// Aggregate allocator state. Counts are pages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Pages between the lowest and highest usable addresses.
    pub span_pages: u64,
    /// Pages the allocator may hand out.
    pub managed_pages: u64,
    /// Managed pages currently allocated.
    pub allocated_pages: u64,
    /// Managed pages currently free.
    pub free_pages: u64,
    /// Span pages that are not managed (reserved ranges and gaps).
    pub reserved_pages: u64,
    /// Lowest managed physical address.
    pub base_address: u64,
    /// Highest managed physical address, end-exclusive.
    pub end_address: u64,
}

/// Free-space fragmentation snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fragmentation {
    pub free_pages: u64,
    pub free_runs: u64,
    pub largest_free_run: u64,
}

/// A bitmap physical page allocator over `WORDS` 64-bit words per bitmap.
pub struct PhysicalAllocator<const WORDS: usize> {
    managed: [u64; WORDS],
    allocated: [u64; WORDS],
    base_page: u64,
    span_pages: u64,
    managed_pages: u64,
    allocated_pages: u64,
    hint_page: u64,
}

impl<const WORDS: usize> PhysicalAllocator<WORDS> {
    /// A zeroed allocator with no managed memory. Useful as a `static`
    /// initializer; call [`init`](Self::init) before allocating.
    pub const fn empty() -> Self {
        Self {
            managed: [0; WORDS],
            allocated: [0; WORDS],
            base_page: 0,
            span_pages: 0,
            managed_pages: 0,
            allocated_pages: 0,
            hint_page: 0,
        }
    }

    /// Build an allocator from usable ranges.
    ///
    /// Ranges may be non-contiguous and in any order. Overlapping ranges are
    /// harmless: setting a managed bit twice does not change its count.
    pub fn new(ranges: &[MemoryRange]) -> Result<Self, Error> {
        let mut allocator = Self::empty();
        allocator.init(ranges)?;
        Ok(allocator)
    }

    /// Reinitialize an existing allocator, discarding all previous state.
    pub fn init(&mut self, ranges: &[MemoryRange]) -> Result<(), Error> {
        self.managed.fill(0);
        self.allocated.fill(0);
        self.base_page = 0;
        self.span_pages = 0;
        self.managed_pages = 0;
        self.allocated_pages = 0;
        self.hint_page = 0;

        let mut lowest = u64::MAX;
        let mut highest = 0u64;
        let mut any = false;

        for range in ranges {
            if range.start >= range.end {
                continue;
            }
            let start = round_down(range.start, PAGE_SIZE);
            let end = round_up(range.end, PAGE_SIZE).ok_or(Error::AddressOverflow)?;
            lowest = lowest.min(start);
            highest = highest.max(end);
            any = true;
        }
        if !any {
            return Err(Error::NoMemory);
        }

        let base_page = lowest / PAGE_SIZE;
        let end_page = highest / PAGE_SIZE;
        let span_pages = end_page
            .checked_sub(base_page)
            .ok_or(Error::AddressOverflow)?;
        let required_words = words_for(span_pages);
        if required_words > WORDS {
            return Err(Error::StorageTooSmall {
                required_words,
                available_words: WORDS,
            });
        }

        self.base_page = base_page;
        self.span_pages = span_pages;

        for range in ranges {
            if range.start >= range.end {
                continue;
            }
            let start = round_up(range.start, PAGE_SIZE).ok_or(Error::AddressOverflow)?;
            let end = round_down(range.end, PAGE_SIZE);
            if start >= end {
                continue;
            }
            let first = start / PAGE_SIZE;
            let last = end / PAGE_SIZE;
            for page in first..last {
                let index = page
                    .checked_sub(base_page)
                    .ok_or(Error::AddressOverflow)?;
                if index >= span_pages {
                    continue;
                }
                if !Self::get_bit(&self.managed, index) {
                    Self::set_bit(&mut self.managed, index);
                    self.managed_pages += 1;
                }
            }
        }

        if self.managed_pages == 0 {
            return Err(Error::NoMemory);
        }
        Ok(())
    }

    /// Remove `[start, end)` from the managed set.
    ///
    /// The range is rounded outward to whole pages. A reservation that touches
    /// an allocated page fails without changing any state, so the caller can
    /// report the bug instead of losing track of live memory.
    pub fn reserve_range(&mut self, start: u64, end: u64) -> Result<(), Error> {
        if start >= end {
            return Ok(());
        }
        let start = round_down(start, PAGE_SIZE);
        let end = round_up(end, PAGE_SIZE).ok_or(Error::AddressOverflow)?;
        let first = start / PAGE_SIZE;
        let last = end / PAGE_SIZE;

        // Validate first: a failed reservation must not partially apply.
        for page in first..last {
            let Some(index) = page.checked_sub(self.base_page) else {
                continue;
            };
            if index >= self.span_pages {
                continue;
            }
            if Self::get_bit(&self.allocated, index) {
                return Err(Error::ReserveAllocated);
            }
        }

        for page in first..last {
            let Some(index) = page.checked_sub(self.base_page) else {
                continue;
            };
            if index >= self.span_pages {
                continue;
            }
            if Self::get_bit(&self.managed, index) {
                Self::clear_bit(&mut self.managed, index);
                self.managed_pages -= 1;
            }
        }
        Ok(())
    }

    /// Allocate one 4 KiB page.
    pub fn alloc(&mut self) -> Option<u64> {
        self.alloc_aligned(1, PAGE_SIZE)
    }

    /// Allocate `count` contiguous pages.
    pub fn alloc_pages(&mut self, count: usize) -> Option<u64> {
        self.alloc_aligned(count, PAGE_SIZE)
    }

    /// Allocate `count` contiguous pages whose physical address is a multiple
    /// of `align`.
    ///
    /// `align` must be a power of two and at least [`PAGE_SIZE`]. Invalid
    /// alignment returns `None`; it is a caller bug, not an out-of-memory
    /// condition, and the allocator state is untouched.
    pub fn alloc_aligned(&mut self, count: usize, align: u64) -> Option<u64> {
        if count == 0 || count as u64 > self.span_pages {
            return None;
        }
        if !align.is_power_of_two() || align < PAGE_SIZE {
            return None;
        }
        let align_pages = align / PAGE_SIZE;
        if align_pages == 0 {
            return None;
        }
        let count = count as u64;

        // Scan from the hint to the end, then from the start to the hint. The
        // hint is only a performance aid; the first-fit result stays correct.
        if let Some(page) = self.scan(self.hint_page, self.span_pages, count, align_pages) {
            self.mark_allocated(page, count);
            self.hint_page = (page + count).min(self.span_pages);
            return Some((self.base_page + page) * PAGE_SIZE);
        }
        if self.hint_page != 0 {
            if let Some(page) = self.scan(0, self.hint_page, count, align_pages) {
                self.mark_allocated(page, count);
                self.hint_page = (page + count).min(self.span_pages);
                return Some((self.base_page + page) * PAGE_SIZE);
            }
        }
        None
    }

    /// Free one page.
    pub fn free(&mut self, address: u64) -> Result<(), Error> {
        self.free_pages(address, 1)
    }

    /// Free `count` contiguous pages.
    ///
    /// The whole range is validated before any bit changes. A range containing
    /// an unmanaged page is [`Error::InvalidAddress`]; a range containing a
    /// managed page that is already free is [`Error::DoubleFree`].
    pub fn free_pages(&mut self, address: u64, count: usize) -> Result<(), Error> {
        if count == 0 {
            return Err(Error::InvalidAddress);
        }
        if address % PAGE_SIZE != 0 {
            return Err(Error::Misaligned);
        }
        let page = address / PAGE_SIZE;
        let index = page
            .checked_sub(self.base_page)
            .ok_or(Error::InvalidAddress)?;
        if index >= self.span_pages || count as u64 > self.span_pages - index {
            return Err(Error::InvalidAddress);
        }

        for offset in 0..count as u64 {
            let slot = index + offset;
            if !Self::get_bit(&self.managed, slot) {
                return Err(Error::InvalidAddress);
            }
            if !Self::get_bit(&self.allocated, slot) {
                return Err(Error::DoubleFree);
            }
        }

        for offset in 0..count as u64 {
            Self::clear_bit(&mut self.allocated, index + offset);
            self.allocated_pages -= 1;
        }
        if index < self.hint_page {
            self.hint_page = index;
        }
        Ok(())
    }

    /// True when the address is inside the managed span and its page is
    /// managed (whether or not it is currently allocated).
    pub fn is_managed(&self, address: u64) -> bool {
        let page = address / PAGE_SIZE;
        let Some(index) = page.checked_sub(self.base_page) else {
            return false;
        };
        index < self.span_pages && Self::get_bit(&self.managed, index)
    }

    /// True when the address is inside the span, regardless of management.
    pub fn in_span(&self, address: u64) -> bool {
        let page = address / PAGE_SIZE;
        page >= self.base_page
            && page
                .checked_sub(self.base_page)
                .map(|index| index < self.span_pages)
                .unwrap_or(false)
    }

    /// Current accounting.
    pub fn stats(&self) -> Stats {
        Stats {
            span_pages: self.span_pages,
            managed_pages: self.managed_pages,
            allocated_pages: self.allocated_pages,
            free_pages: self.managed_pages - self.allocated_pages,
            reserved_pages: self.span_pages - self.managed_pages,
            base_address: self.base_page * PAGE_SIZE,
            end_address: (self.base_page + self.span_pages) * PAGE_SIZE,
        }
    }

    /// Scan the free bitmap. This is `O(span)` and intended for reporting and
    /// tests, not for the allocation hot path.
    pub fn fragmentation(&self) -> Fragmentation {
        let mut result = Fragmentation::default();
        let mut run = 0u64;
        for page in 0..self.span_pages {
            let free = Self::get_bit(&self.managed, page) && !Self::get_bit(&self.allocated, page);
            if free {
                result.free_pages += 1;
                run += 1;
                result.largest_free_run = result.largest_free_run.max(run);
            } else if run != 0 {
                result.free_runs += 1;
                run = 0;
            }
        }
        if run != 0 {
            result.free_runs += 1;
        }
        result
    }

    fn scan(&self, start: u64, end: u64, count: u64, align_pages: u64) -> Option<u64> {
        let mut page = start;
        while page < end {
            // Skip a whole word when nothing in it is free. This keeps the
            // common "mostly allocated" scan cheap without a free list.
            let word = (page / 64) as usize;
            if word >= WORDS {
                break;
            }
            let free_word = self.managed[word] & !self.allocated[word];
            if free_word == 0 {
                let next = (word as u64 + 1) * 64;
                if next <= page {
                    page += 1;
                } else {
                    page = next;
                }
                continue;
            }

            if !self.is_free(page) {
                page += 1;
                continue;
            }

            // Move to the next address that satisfies the alignment.
            let physical = self.base_page + page;
            let misalign = physical % align_pages;
            if misalign != 0 {
                let delta = align_pages - misalign;
                page = match page.checked_add(delta) {
                    Some(next) => next,
                    None => return None,
                };
                continue;
            }

            if page + count > end {
                return None;
            }
            if self.run_is_free(page, count) {
                return Some(page);
            }

            // The free run is too short. Skip to the end of this run rather
            // than testing every page inside it.
            let mut next = page;
            while next < end && self.is_free(next) {
                next += 1;
            }
            page = next;
        }
        None
    }

    fn run_is_free(&self, start: u64, count: u64) -> bool {
        for page in start..start + count {
            if !self.is_free(page) {
                return false;
            }
        }
        true
    }

    fn mark_allocated(&mut self, start: u64, count: u64) {
        for page in start..start + count {
            if !Self::get_bit(&self.allocated, page) {
                Self::set_bit(&mut self.allocated, page);
                self.allocated_pages += 1;
            }
        }
    }

    fn is_free(&self, page: u64) -> bool {
        Self::get_bit(&self.managed, page) && !Self::get_bit(&self.allocated, page)
    }

    fn get_bit(bitmap: &[u64; WORDS], index: u64) -> bool {
        let word = (index / 64) as usize;
        if word >= WORDS {
            return false;
        }
        bitmap[word] & (1u64 << (index % 64)) != 0
    }

    fn set_bit(bitmap: &mut [u64; WORDS], index: u64) {
        let word = (index / 64) as usize;
        if word < WORDS {
            bitmap[word] |= 1u64 << (index % 64);
        }
    }

    fn clear_bit(bitmap: &mut [u64; WORDS], index: u64) {
        let word = (index / 64) as usize;
        if word < WORDS {
            bitmap[word] &= !(1u64 << (index % 64));
        }
    }
}

fn words_for(pages: u64) -> usize {
    ((pages + 63) / 64) as usize
}

fn round_down(value: u64, align: u64) -> u64 {
    value & !(align - 1)
}

fn round_up(value: u64, align: u64) -> Option<u64> {
    if value == 0 {
        return Some(0);
    }
    value.checked_add(align - 1).map(|v| v & !(align - 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    fn range(start: u64, pages: u64) -> MemoryRange {
        MemoryRange::new(start, start + pages * PAGE_SIZE)
    }

    #[test]
    fn allocates_and_frees_pages() {
        let mut allocator = PhysicalAllocator::<16>::new(&[range(0x10_0000, 64)]).unwrap();
        let stats = allocator.stats();
        assert_eq!(stats.managed_pages, 64);
        assert_eq!(stats.free_pages, 64);
        assert_eq!(stats.allocated_pages, 0);

        let first = allocator.alloc().unwrap();
        let second = allocator.alloc().unwrap();
        assert_eq!(first % PAGE_SIZE, 0);
        assert_ne!(first, second);
        assert!(allocator.is_managed(first));
        assert_eq!(allocator.stats().allocated_pages, 2);

        allocator.free(first).unwrap();
        allocator.free(second).unwrap();
        assert_eq!(allocator.stats().allocated_pages, 0);
        assert_eq!(allocator.stats().free_pages, 64);
    }

    #[test]
    fn honours_alignment() {
        let mut allocator = PhysicalAllocator::<64>::new(&[range(0x1_0000, 48)]).unwrap();
        for _ in 0..3 {
            let address = allocator.alloc_aligned(1, 0x1_0000).unwrap();
            assert_eq!(address % 0x1_0000, 0);
        }
        assert_eq!(allocator.alloc_aligned(1, 0x1_0000), None);
    }

    #[test]
    fn rejects_bad_alignment_without_changing_state() {
        let mut allocator = PhysicalAllocator::<16>::new(&[range(0x10_0000, 8)]).unwrap();
        let before = allocator.stats();
        assert_eq!(allocator.alloc_aligned(1, 3), None);
        assert_eq!(allocator.alloc_aligned(1, 2048), None);
        assert_eq!(allocator.alloc_aligned(1, 8191), None);
        assert_eq!(allocator.stats(), before);
    }

    #[test]
    fn exhaustion_and_recovery() {
        let mut allocator = PhysicalAllocator::<16>::new(&[range(0x20_0000, 64)]).unwrap();
        let mut pages = Vec::new();
        while let Some(page) = allocator.alloc() {
            pages.push(page);
        }
        assert_eq!(pages.len(), 64);
        assert_eq!(allocator.alloc(), None);
        assert_eq!(allocator.stats().free_pages, 0);

        allocator.free(pages[17]).unwrap();
        assert_eq!(allocator.stats().free_pages, 1);
        let recovered = allocator.alloc().unwrap();
        assert_eq!(recovered, pages[17]);
    }

    #[test]
    fn detects_invalid_and_double_free() {
        let mut allocator = PhysicalAllocator::<16>::new(&[range(0x30_0000, 8)]).unwrap();
        let page = allocator.alloc().unwrap();

        assert_eq!(allocator.free(0), Err(Error::InvalidAddress));
        assert_eq!(allocator.free(page + 1), Err(Error::Misaligned));
        assert_eq!(
            allocator.free(0x30_0000 + 64 * PAGE_SIZE),
            Err(Error::InvalidAddress)
        );
        assert_eq!(allocator.free(page + PAGE_SIZE), Err(Error::DoubleFree));
        assert_eq!(allocator.free(page), Ok(()));
        assert_eq!(allocator.free(page), Err(Error::DoubleFree));
    }

    #[test]
    fn reserved_pages_are_never_allocated() {
        let mut allocator = PhysicalAllocator::<16>::new(&[range(0x40_0000, 64)]).unwrap();
        let reserved_start = 0x40_0000 + 16 * PAGE_SIZE;
        let reserved_end = reserved_start + 8 * PAGE_SIZE;
        allocator.reserve_range(reserved_start, reserved_end).unwrap();

        let mut pages = Vec::new();
        while let Some(page) = allocator.alloc() {
            pages.push(page);
        }
        assert_eq!(pages.len(), 56);
        for page in &pages {
            assert!(!(*page >= reserved_start && *page < reserved_end));
        }
        assert!(!allocator.is_managed(reserved_start));
        assert_eq!(allocator.free(reserved_start), Err(Error::InvalidAddress));
        assert_eq!(allocator.stats().reserved_pages, 8);
    }

    #[test]
    fn reservation_cannot_take_allocated_memory() {
        let mut allocator = PhysicalAllocator::<16>::new(&[range(0x50_0000, 8)]).unwrap();
        let page = allocator.alloc().unwrap();
        assert_eq!(
            allocator.reserve_range(page, page + PAGE_SIZE),
            Err(Error::ReserveAllocated)
        );
        assert!(allocator.is_managed(page));
        assert_eq!(allocator.stats().allocated_pages, 1);
    }

    #[test]
    fn non_contiguous_ranges_are_independent() {
        let mut allocator = PhysicalAllocator::<16>::new(&[
            range(0x60_0000, 2),
            range(0x61_0000, 2),
        ])
        .unwrap();
        assert_eq!(allocator.stats().managed_pages, 4);
        assert_eq!(allocator.stats().span_pages, 18);

        let mut pages = Vec::new();
        while let Some(page) = allocator.alloc() {
            pages.push(page);
        }
        pages.sort();
        assert_eq!(
            pages,
            std::vec![
                0x60_0000,
                0x60_0000 + PAGE_SIZE,
                0x61_0000,
                0x61_0000 + PAGE_SIZE
            ]
        );
        for page in pages {
            allocator.free(page).unwrap();
        }
    }

    #[test]
    fn fragmentation_is_reported() {
        let mut allocator = PhysicalAllocator::<16>::new(&[range(0x70_0000, 16)]).unwrap();
        let mut pages = Vec::new();
        while let Some(page) = allocator.alloc() {
            pages.push(page);
        }
        for index in (0..pages.len()).step_by(2) {
            allocator.free(pages[index]).unwrap();
        }
        let fragmentation = allocator.fragmentation();
        assert_eq!(fragmentation.free_pages, 8);
        assert_eq!(fragmentation.free_runs, 8);
        assert_eq!(fragmentation.largest_free_run, 1);

        for index in (1..pages.len()).step_by(2) {
            allocator.free(pages[index]).unwrap();
        }
        let fragmentation = allocator.fragmentation();
        assert_eq!(fragmentation.free_pages, 16);
        assert_eq!(fragmentation.free_runs, 1);
        assert_eq!(fragmentation.largest_free_run, 16);
    }

    #[test]
    fn storage_too_small_is_reported() {
        let result = PhysicalAllocator::<1>::new(&[range(0x80_0000, 65)]);
        assert_eq!(
            result.err(),
            Some(Error::StorageTooSmall {
                required_words: 2,
                available_words: 1,
            })
        );
    }
}

