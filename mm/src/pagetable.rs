//! Shared AArch64 VMSAv8-64 stage-1 page-table walker.
//!
//! This module is used by three environments with different views of memory:
//!
//! * the UEFI loader, which builds the bootstrap tables under an identity map,
//! * the kernel, which builds its final map through the physical map while it
//!   runs in the higher half,
//! * host tests, which use ordinary heap memory.
//!
//! The walker never assumes that a table's physical address is directly
//! dereferenceable. [`TableMemory`] converts a physical table address to a
//! writable pointer, and [`TableAllocator`] supplies zeroed 4 KiB tables.
//!
//! # Granularity
//!
//! The walker supports the three legal stage-1 descriptors at this granule:
//!
//! | Level | Block size | Descriptor |
//! | --- | --- | --- |
//! | 1 | 1 GiB | block (`0b01`) |
//! | 2 | 2 MiB | block (`0b01`) |
//! | 3 | 4 KiB | page  (`0b11`) |
//!
//! [`Walker::map_range`] picks the largest descriptor whose virtual and
//! physical addresses are aligned and whose size fits. A finer mapping that is
//! created later splits a block by allocating a new table and copying the
//! block's attributes into every child. A coarser mapping replaces whatever is
//! below it; the orphaned tables are leaked rather than tracked, because all
//! three users either abandon their pool at the end of the job (loader) or keep
//! it for the lifetime of the kernel (kernel).
//!
//! # Cache and TLB maintenance
//!
//! The walker only writes descriptors. After changing a live mapping the caller
//! must perform the architectural sequence:
//!
//! ```text
//! dsb ishst
//! tlbi vaae1, <VA/4096>      // or vmalle1 for the whole regime
//! dsb ish
//! isb
//! ```
//!
//! The loader and kernel wrappers do this. Host tests do not need it.
//!
//! # W^X
//!
//! The walker does not reject a mapping that is both writable and executable.
//! W^X is a policy of the caller: the kernel final map checks it, and the
//! loader's boot trampoline is deliberately RWX because it is discarded before
//! the kernel reaches Rust.

use core::ptr;

/// Page size for this granule.
pub const PAGE_SIZE: u64 = 4096;
/// Level-2 block size.
pub const BLOCK_2M: u64 = 2 * 1024 * 1024;
/// Level-1 block size.
pub const BLOCK_1G: u64 = 1024 * 1024 * 1024;
/// Entries per translation table.
pub const TABLE_ENTRIES: usize = 512;

/// Output-address bits of a descriptor.
pub const ADDRESS_MASK: u64 = 0x0000_FFFF_FFFF_F000;
/// Access flag: clear means the first access faults.
pub const AF: u64 = 1 << 10;
/// Privileged execute-never.
pub const PXN: u64 = 1 << 53;
/// Unprivileged execute-never.
pub const UXN: u64 = 1 << 54;

const DESC_BLOCK: u64 = 0b01;
const DESC_TABLE: u64 = 0b11;
const DESC_PAGE: u64 = 0b11;

const SH_INNER: u64 = 0b11 << 8;
/// `AP[2]`: set means read-only at EL1 (and EL0).
const AP_RO: u64 = 1 << 7;
/// `AP[1]`: set means EL0 may access the page.
const AP_USER: u64 = 1 << 6;
const NG: u64 = 1 << 11;

/// MAIR attribute index for normal write-back memory.
pub const MAIR_NORMAL_INDEX: u64 = 0;
/// MAIR attribute index for Device-nGnRnE memory.
pub const MAIR_DEVICE_INDEX: u64 = 1;
/// MAIR attribute index for normal non-cacheable memory.
pub const MAIR_NORMAL_NC_INDEX: u64 = 2;

/// `MAIR_EL1` value matching the three indices above.
///
/// * Attr0 = `0xff`: Normal, inner/outer write-back, read/write allocate.
/// * Attr1 = `0x00`: Device-nGnRnE.
/// * Attr2 = `0x44`: Normal, inner/outer non-cacheable.
pub const MAIR_VALUE: u64 = 0xff | (0x00 << 8) | (0x44 << 16);

/// AArch64 `TCR_EL1` value for a 48-bit VA split between TTBR0 and TTBR1.
///
/// `ips` is the 3-bit `TCR_EL1.IPS` encoding of the supported physical address
/// range. `asid16` selects 16-bit ASIDs; when false the AS field stays zero and
/// TTBR ASIDs carry eight bits.
pub const fn tcr_value(asid16: bool, ips: u64) -> u64 {
    let mut value = 0;
    // T0SZ = T1SZ = 16 -> 48-bit VA per regime.
    value |= 16;
    value |= 16 << 16;
    // IRGN0/ORGN0 = write-back write-allocate, SH0 = inner shareable.
    value |= 0b01 << 8;
    value |= 0b01 << 10;
    value |= 0b11 << 12;
    // IRGN1/ORGN1 = write-back write-allocate, SH1 = inner shareable.
    value |= 0b01 << 24;
    value |= 0b01 << 26;
    value |= 0b11 << 28;
    // TG0 = 4 KiB (0b00), TG1 = 4 KiB (0b10).
    // value |= 0b00 << 14;
    value |= 0b10 << 30;
    // EPD0 = EPD1 = 0, A1 = 0 (ASID comes from TTBR0).
    value |= (ips & 0x7) << 32;
    if asid16 {
        value |= 1 << 36;
    }
    value
}

/// ASID width reported by `ID_AA64MMFR0_EL1.ASIDBits`.
pub const fn asid_width(mmfr0: u64) -> u32 {
    match (mmfr0 >> 4) & 0xf {
        0 => 8,
        1 => 16,
        _ => 8,
    }
}

/// Access permissions and memory attributes for one mapping.
///
/// There is no stage-1 "read disabled" bit: any valid descriptor is readable.
/// `read` is kept so callers can state their intent and so a decoded mapping
/// round-trips through [`Translation`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Perm {
    pub read: bool,
    pub write: bool,
    pub exec: bool,
    /// Reachable from EL0 (`AP[1] = 1`). User pages are never executable at
    /// EL1 (`PXN = 1`).
    pub user: bool,
    /// Device-nGnRnE instead of normal write-back.
    pub device: bool,
    /// Normal non-cacheable instead of normal write-back.
    pub normal_nc: bool,
    /// Global mapping (`nG = 0`). Kernel mappings are global; user mappings
    /// are tagged by the ASID in TTBR0.
    pub global: bool,
}

impl Perm {
    /// Kernel instruction page: read-only, executable at EL1.
    pub const KERNEL_RX: Self = Self {
        read: true,
        write: false,
        exec: true,
        user: false,
        device: false,
        normal_nc: false,
        global: true,
    };

    /// Kernel read-only data.
    pub const KERNEL_RO: Self = Self {
        read: true,
        write: false,
        exec: false,
        user: false,
        device: false,
        normal_nc: false,
        global: true,
    };

    /// Kernel data, BSS and stacks: writable, never executable.
    pub const KERNEL_RW: Self = Self {
        read: true,
        write: true,
        exec: false,
        user: false,
        device: false,
        normal_nc: false,
        global: true,
    };

    /// User instruction page.
    pub const USER_RX: Self = Self {
        read: true,
        write: false,
        exec: true,
        user: true,
        device: false,
        normal_nc: false,
        global: false,
    };

    /// User read-only data.
    pub const USER_RO: Self = Self {
        read: true,
        write: false,
        exec: false,
        user: true,
        device: false,
        normal_nc: false,
        global: false,
    };

    /// User data and stack: writable, never executable.
    pub const USER_RW: Self = Self {
        read: true,
        write: true,
        exec: false,
        user: true,
        device: false,
        normal_nc: false,
        global: false,
    };

    /// Device MMIO visible to the kernel.
    pub const DEVICE: Self = Self {
        read: true,
        write: true,
        exec: false,
        user: false,
        device: true,
        normal_nc: false,
        global: true,
    };

    /// Normal non-cacheable memory such as the framebuffer.
    pub const NORMAL_NC: Self = Self {
        read: true,
        write: true,
        exec: false,
        user: false,
        device: false,
        normal_nc: true,
        global: true,
    };

    /// Temporary identity-map permission used by the loader. The mapping is
    /// discarded when the boot core leaves the identity map.
    pub const BOOT_RWX: Self = Self {
        read: true,
        write: true,
        exec: true,
        user: false,
        device: false,
        normal_nc: false,
        global: true,
    };

    /// True when the mapping would be writable and executable at the same
    /// privilege level. The kernel final map must contain none of these.
    pub const fn is_writable_executable(&self) -> bool {
        self.write && self.exec
    }
}

/// A decoded leaf mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Translation {
    /// Physical address, including the offset within the leaf.
    pub physical: u64,
    /// `1` for a 1 GiB block, `2` for a 2 MiB block, `3` for a 4 KiB page.
    pub level: u8,
    pub perm: Perm,
}

/// Failures reported by the walker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The virtual or physical address, or the size, is not page aligned.
    Misaligned,
    /// The physical address or mapped span is outside the 48-bit model.
    OutOfRange,
    /// The table allocator has no space left.
    TableAllocation,
    /// No leaf mapping covers the address.
    NotMapped,
    /// A table entry is neither a valid table nor a valid block.
    Conflict,
}

impl Error {
    pub const fn message(self) -> &'static str {
        match self {
            Error::Misaligned => "address is not page aligned",
            Error::OutOfRange => "address is outside the 48-bit physical space",
            Error::TableAllocation => "out of page tables",
            Error::NotMapped => "address is not mapped",
            Error::Conflict => "page-table entry is neither a table nor a block",
        }
    }
}

/// Converts a physical table address to a writable table pointer.
pub trait TableMemory {
    /// Return a pointer to the 512-entry table at `physical`.
    ///
    /// The pointer must remain valid for as long as the table exists.
    fn table(&self, physical: u64) -> *mut u64;
}

/// Supplies zeroed translation tables.
pub trait TableAllocator {
    /// Allocate one zeroed, 4 KiB-aligned physical page for a table.
    fn alloc_table(&mut self) -> Option<u64>;
}

/// A stage-1 table walker over a [`TableMemory`] implementation.
pub struct Walker<M> {
    memory: M,
}

impl<M: TableMemory> Walker<M> {
    pub const fn new(memory: M) -> Self {
        Self { memory }
    }

    pub fn memory(&self) -> &M {
        &self.memory
    }

    /// Map one 4 KiB page.
    pub fn map_page<A: TableAllocator>(
        &self,
        root: u64,
        virtual_address: u64,
        physical_address: u64,
        perm: Perm,
        allocator: &mut A,
    ) -> Result<(), Error> {
        self.check_page(virtual_address, physical_address)?;
        let l1 = self.ensure_table(root, Self::index(virtual_address, 0), 0, allocator)?;
        let l2 = self.ensure_table(l1, Self::index(virtual_address, 1), 1, allocator)?;
        let l3 = self.ensure_table(l2, Self::index(virtual_address, 2), 2, allocator)?;
        let entry = (physical_address & ADDRESS_MASK) | lower_attributes(perm) | DESC_PAGE;
        self.write(l3, Self::index(virtual_address, 3), entry);
        Ok(())
    }

    /// Map one 2 MiB block. Both addresses must be 2 MiB aligned.
    pub fn map_block_2m<A: TableAllocator>(
        &self,
        root: u64,
        virtual_address: u64,
        physical_address: u64,
        perm: Perm,
        allocator: &mut A,
    ) -> Result<(), Error> {
        self.check_block(virtual_address, physical_address, BLOCK_2M)?;
        let l1 = self.ensure_table(root, Self::index(virtual_address, 0), 0, allocator)?;
        let l2 = self.ensure_table(l1, Self::index(virtual_address, 1), 1, allocator)?;
        let entry = (physical_address & ADDRESS_MASK) | lower_attributes(perm) | DESC_BLOCK;
        self.write(l2, Self::index(virtual_address, 2), entry);
        Ok(())
    }

    /// Map one 1 GiB block. Both addresses must be 1 GiB aligned.
    pub fn map_block_1g<A: TableAllocator>(
        &self,
        root: u64,
        virtual_address: u64,
        physical_address: u64,
        perm: Perm,
        allocator: &mut A,
    ) -> Result<(), Error> {
        self.check_block(virtual_address, physical_address, BLOCK_1G)?;
        let l1 = self.ensure_table(root, Self::index(virtual_address, 0), 0, allocator)?;
        let entry = (physical_address & ADDRESS_MASK) | lower_attributes(perm) | DESC_BLOCK;
        self.write(l1, Self::index(virtual_address, 1), entry);
        Ok(())
    }

    /// Map `[virtual_address, virtual_address + size)` with the largest
    /// descriptors that are aligned.
    ///
    /// A block mapping replaces any finer mappings inside the same block.
    pub fn map_range<A: TableAllocator>(
        &self,
        root: u64,
        mut virtual_address: u64,
        mut physical_address: u64,
        mut size: u64,
        perm: Perm,
        allocator: &mut A,
    ) -> Result<(), Error> {
        if size & (PAGE_SIZE - 1) != 0 {
            return Err(Error::Misaligned);
        }
        while size > 0 {
            if virtual_address & (BLOCK_1G - 1) == 0
                && physical_address & (BLOCK_1G - 1) == 0
                && size >= BLOCK_1G
            {
                self.map_block_1g(root, virtual_address, physical_address, perm, allocator)?;
                virtual_address = virtual_address.wrapping_add(BLOCK_1G);
                physical_address = physical_address.wrapping_add(BLOCK_1G);
                size -= BLOCK_1G;
            } else if virtual_address & (BLOCK_2M - 1) == 0
                && physical_address & (BLOCK_2M - 1) == 0
                && size >= BLOCK_2M
            {
                self.map_block_2m(root, virtual_address, physical_address, perm, allocator)?;
                virtual_address = virtual_address.wrapping_add(BLOCK_2M);
                physical_address = physical_address.wrapping_add(BLOCK_2M);
                size -= BLOCK_2M;
            } else {
                self.map_page(root, virtual_address, physical_address, perm, allocator)?;
                virtual_address = virtual_address.wrapping_add(PAGE_SIZE);
                physical_address = physical_address.wrapping_add(PAGE_SIZE);
                size -= PAGE_SIZE;
            }
        }
        Ok(())
    }

    /// Remove one page. A larger mapping that covers the page is split first.
    pub fn unmap_page<A: TableAllocator>(
        &self,
        root: u64,
        virtual_address: u64,
        allocator: &mut A,
    ) -> Result<(), Error> {
        if virtual_address & (PAGE_SIZE - 1) != 0 {
            return Err(Error::Misaligned);
        }
        let l3 = self.leaf_table(root, virtual_address, allocator)?;
        let index = Self::index(virtual_address, 3);
        if self.read(l3, index) & 1 == 0 {
            return Err(Error::NotMapped);
        }
        self.write(l3, index, 0);
        Ok(())
    }

    /// Change the permissions of one page, preserving its physical address.
    pub fn protect_page<A: TableAllocator>(
        &self,
        root: u64,
        virtual_address: u64,
        perm: Perm,
        allocator: &mut A,
    ) -> Result<(), Error> {
        if virtual_address & (PAGE_SIZE - 1) != 0 {
            return Err(Error::Misaligned);
        }
        let l3 = self.leaf_table(root, virtual_address, allocator)?;
        let index = Self::index(virtual_address, 3);
        let existing = self.read(l3, index);
        if existing & 1 == 0 {
            return Err(Error::NotMapped);
        }
        let entry = (existing & ADDRESS_MASK) | lower_attributes(perm) | DESC_PAGE;
        self.write(l3, index, entry);
        Ok(())
    }

    /// Look up the leaf mapping that covers `virtual_address`.
    pub fn translate(&self, root: u64, virtual_address: u64) -> Option<Translation> {
        let mut table = root;
        for level in 0..3u8 {
            let descriptor = self.read(table, Self::index(virtual_address, level));
            if descriptor & 1 == 0 {
                return None;
            }
            match descriptor & 0b11 {
                DESC_TABLE => table = descriptor & ADDRESS_MASK,
                DESC_BLOCK => {
                    let shift = 12 + 9 * (3 - level as u32);
                    let base = descriptor & ADDRESS_MASK;
                    let offset = virtual_address & ((1u64 << shift) - 1);
                    return Some(Translation {
                        physical: base + offset,
                        level,
                        perm: decode(descriptor),
                    });
                }
                _ => return None,
            }
        }
        let descriptor = self.read(table, Self::index(virtual_address, 3));
        if descriptor & 1 == 0 || descriptor & 0b11 != DESC_PAGE {
            return None;
        }
        Some(Translation {
            physical: (descriptor & ADDRESS_MASK) + (virtual_address & (PAGE_SIZE - 1)),
            level: 3,
            perm: decode(descriptor),
        })
    }

    /// Return the raw leaf descriptor covering `virtual_address`, for
    /// diagnostics and tests.
    pub fn descriptor_at(&self, root: u64, virtual_address: u64) -> Option<u64> {
        let mut table = root;
        for level in 0..3u8 {
            let descriptor = self.read(table, Self::index(virtual_address, level));
            if descriptor & 1 == 0 {
                return None;
            }
            match descriptor & 0b11 {
                DESC_TABLE => table = descriptor & ADDRESS_MASK,
                DESC_BLOCK => return Some(descriptor),
                _ => return None,
            }
        }
        let descriptor = self.read(table, Self::index(virtual_address, 3));
        if descriptor & 1 == 0 || descriptor & 0b11 != DESC_PAGE {
            None
        } else {
            Some(descriptor)
        }
    }

    /// Number of 4 KiB tables reachable from `root`. Used by boot diagnostics
    /// and tests; walks each table once by relying on the tree shape.
    pub fn table_count(&self, root: u64) -> usize {
        let mut count = 0;
        self.count_tables(root, 0, &mut count);
        count
    }

    fn count_tables(&self, table: u64, level: u8, count: &mut usize) {
        *count += 1;
        if level >= 3 {
            return;
        }
        for index in 0..TABLE_ENTRIES {
            let descriptor = self.read(table, index);
            if descriptor & 0b11 == DESC_TABLE {
                self.count_tables(descriptor & ADDRESS_MASK, level + 1, count);
            }
        }
    }

    /// Walk to the level-3 table covering `virtual_address`, splitting any
    /// larger mapping on the way.
    fn leaf_table<A: TableAllocator>(
        &self,
        root: u64,
        virtual_address: u64,
        allocator: &mut A,
    ) -> Result<u64, Error> {
        let l1 = self.ensure_table(root, Self::index(virtual_address, 0), 0, allocator)?;
        let l2 = self.ensure_table(l1, Self::index(virtual_address, 1), 1, allocator)?;
        self.ensure_table(l2, Self::index(virtual_address, 2), 2, allocator)
    }

    /// Return the next-level table for `parent[index]`, allocating or splitting
    /// as needed.
    ///
    /// `level` is the level of the entry being examined: 0 for an L0 entry,
    /// 1 for an L1 entry and 2 for an L2 entry.
    fn ensure_table<A: TableAllocator>(
        &self,
        parent: u64,
        index: usize,
        level: u8,
        allocator: &mut A,
    ) -> Result<u64, Error> {
        let descriptor = self.read(parent, index);
        if descriptor & 1 == 0 {
            let next = self.allocate(allocator)?;
            self.write(parent, index, next | DESC_TABLE);
            return Ok(next);
        }
        match descriptor & 0b11 {
            DESC_TABLE => Ok(descriptor & ADDRESS_MASK),
            DESC_BLOCK => {
                let next = self.allocate(allocator)?;
                self.split_block(descriptor, level, next);
                self.write(parent, index, next | DESC_TABLE);
                Ok(next)
            }
            _ => Err(Error::Conflict),
        }
    }

    fn allocate<A: TableAllocator>(&self, allocator: &mut A) -> Result<u64, Error> {
        let table = allocator.alloc_table().ok_or(Error::TableAllocation)?;
        if table & (PAGE_SIZE - 1) != 0 || table == 0 {
            return Err(Error::Misaligned);
        }
        for index in 0..TABLE_ENTRIES {
            self.write(table, index, 0);
        }
        Ok(table)
    }

    /// Copy a block descriptor into the 512 children of a newly allocated
    /// table, preserving the block's attributes and output address.
    fn split_block(&self, descriptor: u64, level: u8, table: u64) {
        let base = descriptor & ADDRESS_MASK;
        let attributes = (descriptor & 0x0ffc) | (descriptor & (PXN | UXN));
        let child_shift = 12 + 9 * (2 - level as u32);
        let child_type = if level == 2 { DESC_PAGE } else { DESC_BLOCK };
        for index in 0..TABLE_ENTRIES {
            let address = base + ((index as u64) << child_shift);
            self.write(table, index, address | attributes | child_type);
        }
    }

    fn check_page(&self, virtual_address: u64, physical_address: u64) -> Result<(), Error> {
        if virtual_address & (PAGE_SIZE - 1) != 0 || physical_address & (PAGE_SIZE - 1) != 0 {
            return Err(Error::Misaligned);
        }
        if physical_address >= (1u64 << 48) {
            return Err(Error::OutOfRange);
        }
        Ok(())
    }

    fn check_block(
        &self,
        virtual_address: u64,
        physical_address: u64,
        block: u64,
    ) -> Result<(), Error> {
        if virtual_address & (block - 1) != 0 || physical_address & (block - 1) != 0 {
            return Err(Error::Misaligned);
        }
        if physical_address.checked_add(block).map_or(true, |end| end > (1u64 << 48)) {
            return Err(Error::OutOfRange);
        }
        Ok(())
    }

    fn index(virtual_address: u64, level: u8) -> usize {
        ((virtual_address >> (39 - 9 * level as u32)) & 0x1ff) as usize
    }

    fn read(&self, table: u64, index: usize) -> u64 {
        debug_assert!(index < TABLE_ENTRIES);
        // SAFETY: the allocator contract guarantees a valid 512-entry table,
        // and `index` is bounded by the caller's virtual address.
        unsafe { ptr::read_volatile(self.memory.table(table).add(index)) }
    }

    fn write(&self, table: u64, index: usize, value: u64) {
        debug_assert!(index < TABLE_ENTRIES);
        // SAFETY: see `read`.
        unsafe { ptr::write_volatile(self.memory.table(table).add(index), value) }
    }
}

/// Encode the lower/upper permission bits for `perm`.
pub const fn lower_attributes(perm: Perm) -> u64 {
    let index = if perm.device {
        MAIR_DEVICE_INDEX
    } else if perm.normal_nc {
        MAIR_NORMAL_NC_INDEX
    } else {
        MAIR_NORMAL_INDEX
    };
    let mut value = index << 2;
    value |= AF;
    if !perm.device {
        value |= SH_INNER;
    }
    if !perm.write {
        value |= AP_RO;
    }
    if perm.user {
        value |= AP_USER;
    }
    if !perm.global {
        value |= NG;
    }
    // EL1 may execute a page only when it is marked executable and is not a
    // user page. EL0 may execute it only when it is marked executable.
    if !perm.exec || perm.user {
        value |= PXN;
    }
    if !perm.exec || !perm.user {
        value |= UXN;
    }
    value
}

fn decode(descriptor: u64) -> Perm {
    let attribute_index = (descriptor >> 2) & 0x7;
    let read_only = (descriptor >> 7) & 1 != 0;
    let user = (descriptor >> 6) & 1 != 0;
    let pxn = descriptor & PXN != 0;
    let uxn = descriptor & UXN != 0;
    Perm {
        read: true,
        write: !read_only,
        exec: if user { !uxn } else { !pxn },
        user,
        device: attribute_index == MAIR_DEVICE_INDEX,
        normal_nc: attribute_index == MAIR_NORMAL_NC_INDEX,
        global: descriptor & NG == 0,
    }
}

const _: () = assert!(MAIR_VALUE == 0x0044_00ff);
const _: () = assert!(BLOCK_2M == 1 << 21);
const _: () = assert!(BLOCK_1G == 1 << 30);

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::vec::Vec;

    const ROOT_BASE: u64 = 0x1000_0000;

    #[derive(Clone)]
    struct Tables {
        tables: Rc<RefCell<Vec<Box<[u64; TABLE_ENTRIES]>>>>,
    }

    impl Tables {
        fn new() -> Self {
            Self {
                tables: Rc::new(RefCell::new(Vec::new())),
            }
        }
    }

    struct Memory(Tables);

    impl TableMemory for Memory {
        fn table(&self, physical: u64) -> *mut u64 {
            let index = ((physical - ROOT_BASE) / PAGE_SIZE) as usize;
            let mut tables = self.0.tables.borrow_mut();
            tables[index].as_mut_ptr()
        }
    }

    struct Allocator {
        tables: Tables,
        limit: usize,
    }

    impl TableAllocator for Allocator {
        fn alloc_table(&mut self) -> Option<u64> {
            let mut tables = self.tables.tables.borrow_mut();
            let index = tables.len();
            if index >= self.limit {
                return None;
            }
            tables.push(Box::new([0u64; TABLE_ENTRIES]));
            Some(ROOT_BASE + index as u64 * PAGE_SIZE)
        }
    }

    fn setup(limit: usize) -> (Walker<Memory>, Allocator, u64) {
        let tables = Tables::new();
        let memory = Memory(tables.clone());
        let mut allocator = Allocator { tables, limit };
        let root = allocator.alloc_table().expect("root table");
        (Walker::new(memory), allocator, root)
    }

    #[test]
    fn map_page_translates_and_unmaps() {
        let (walker, mut allocator, root) = setup(64);
        walker
            .map_page(root, 0x1_0000, 0x8000_0000, Perm::KERNEL_RW, &mut allocator)
            .unwrap();

        let translation = walker.translate(root, 0x1_0000).unwrap();
        assert_eq!(translation.physical, 0x8000_0000);
        assert_eq!(translation.level, 3);
        assert!(translation.perm.write);
        assert!(!translation.perm.exec);
        assert!(translation.perm.global);

        walker.unmap_page(root, 0x1_0000, &mut allocator).unwrap();
        assert!(walker.translate(root, 0x1_0000).is_none());
        assert_eq!(
            walker.unmap_page(root, 0x1_0000, &mut allocator),
            Err(Error::NotMapped)
        );
    }

    #[test]
    fn range_uses_blocks_and_maps_offsets() {
        let (walker, mut allocator, root) = setup(64);
        walker
            .map_range(
                root,
                0x20_0000,
                0x4000_0000,
                4 * BLOCK_2M,
                Perm::DEVICE,
                &mut allocator,
            )
            .unwrap();

        let first = walker.translate(root, 0x20_0000 + 0x1234).unwrap();
        assert_eq!(first.physical, 0x4000_0000 + 0x1234);
        assert_eq!(first.level, 2);
        assert!(first.perm.device);

        let last = walker.translate(root, 0x5f_ffff).unwrap();
        assert_eq!(last.physical, 0x4000_0000 + 0x3f_ffff);
        assert_eq!(last.level, 2);
    }

    #[test]
    fn page_mapping_splits_a_block() {
        let (walker, mut allocator, root) = setup(64);
        walker
            .map_block_2m(root, 0x20_0000, 0x4000_0000, Perm::DEVICE, &mut allocator)
            .unwrap();
        walker
            .map_page(root, 0x20_1000, 0x8000_0000, Perm::KERNEL_RW, &mut allocator)
            .unwrap();

        let page = walker.translate(root, 0x20_1000).unwrap();
        assert_eq!(page.physical, 0x8000_0000);
        assert_eq!(page.level, 3);

        let neighbour = walker.translate(root, 0x20_0000).unwrap();
        assert_eq!(neighbour.physical, 0x4000_0000);
        assert_eq!(neighbour.level, 3);
        assert!(neighbour.perm.device);
    }

    #[test]
    fn protect_preserves_the_physical_address() {
        let (walker, mut allocator, root) = setup(64);
        walker
            .map_page(root, 0x1_0000, 0x8000_0000, Perm::KERNEL_RW, &mut allocator)
            .unwrap();
        walker
            .protect_page(root, 0x1_0000, Perm::KERNEL_RX, &mut allocator)
            .unwrap();

        let translation = walker.translate(root, 0x1_0000).unwrap();
        assert_eq!(translation.physical, 0x8000_0000);
        assert!(!translation.perm.write);
        assert!(translation.perm.exec);
    }

    #[test]
    fn coarse_mapping_promotes_a_region() {
        let (walker, mut allocator, root) = setup(64);
        walker
            .map_block_1g(root, 0, 0, Perm::DEVICE, &mut allocator)
            .unwrap();
        walker
            .map_block_2m(root, 0x40_0000, 0x40_0000, Perm::NORMAL_NC, &mut allocator)
            .unwrap();

        let promoted = walker.translate(root, 0x40_0000).unwrap();
        assert!(promoted.perm.normal_nc);
        assert_eq!(promoted.level, 2);

        let untouched = walker.translate(root, 0x20_0000).unwrap();
        assert!(untouched.perm.device);
        assert_eq!(untouched.level, 2);
    }

    #[test]
    fn unmap_page_splits_a_one_gigabyte_block() {
        let (walker, mut allocator, root) = setup(64);
        walker
            .map_block_1g(root, 0, 0, Perm::DEVICE, &mut allocator)
            .unwrap();
        walker.unmap_page(root, 0x12_3000, &mut allocator).unwrap();

        assert!(walker.translate(root, 0x12_3000).is_none());
        let neighbour = walker.translate(root, 0x12_4000).unwrap();
        assert_eq!(neighbour.physical, 0x12_4000);
        assert!(neighbour.perm.device);
    }

    #[test]
    fn kernel_high_addresses_translate() {
        let (walker, mut allocator, root) = setup(64);
        let virtual_address = 0xffff_8000_0001_0000u64;
        walker
            .map_page(
                root,
                virtual_address,
                0x1234_5000,
                Perm::KERNEL_RX,
                &mut allocator,
            )
            .unwrap();
        let translation = walker.translate(root, virtual_address).unwrap();
        assert_eq!(translation.physical, 0x1234_5000);
        assert!(translation.perm.exec);
        assert!(!translation.perm.write);
    }

    #[test]
    fn w_x_bits_are_encoded_exactly() {
        let (walker, mut allocator, root) = setup(64);
        walker
            .map_page(root, 0x1_0000, 0x8000_0000, Perm::KERNEL_RX, &mut allocator)
            .unwrap();
        walker
            .map_page(root, 0x2_0000, 0x8000_1000, Perm::KERNEL_RO, &mut allocator)
            .unwrap();
        walker
            .map_page(root, 0x3_0000, 0x8000_2000, Perm::KERNEL_RW, &mut allocator)
            .unwrap();
        walker
            .map_page(root, 0x4_0000, 0x8000_3000, Perm::USER_RX, &mut allocator)
            .unwrap();

        let text = walker.descriptor_at(root, 0x1_0000).unwrap();
        assert_eq!(text & PXN, 0);
        assert_ne!(text & UXN, 0);
        assert_ne!(text & AP_RO, 0);

        let rodata = walker.descriptor_at(root, 0x2_0000).unwrap();
        assert_ne!(rodata & PXN, 0);
        assert_ne!(rodata & UXN, 0);

        let data = walker.descriptor_at(root, 0x3_0000).unwrap();
        assert_eq!(data & AP_RO, 0);
        assert_ne!(data & PXN, 0);
        assert_ne!(data & UXN, 0);

        let user_text = walker.descriptor_at(root, 0x4_0000).unwrap();
        assert_ne!(user_text & PXN, 0);
        assert_eq!(user_text & UXN, 0);
        assert_ne!(user_text & AP_USER, 0);
        assert_ne!(user_text & NG, 0);
    }

    #[test]
    fn table_allocation_is_bounded() {
        let (walker, mut allocator, root) = setup(1);
        assert_eq!(
            walker.map_page(root, 0x1_0000, 0x8000_0000, Perm::KERNEL_RW, &mut allocator),
            Err(Error::TableAllocation)
        );
    }

    #[test]
    fn unaligned_and_out_of_range_are_rejected() {
        let (walker, mut allocator, root) = setup(64);
        assert_eq!(
            walker.map_page(root, 0x1001, 0x8000_0000, Perm::KERNEL_RW, &mut allocator),
            Err(Error::Misaligned)
        );
        assert_eq!(
            walker.map_page(root, 0x1000, 0x1001, Perm::KERNEL_RW, &mut allocator),
            Err(Error::Misaligned)
        );
        assert_eq!(
            walker.map_page(
                root,
                0x1000,
                1u64 << 48,
                Perm::KERNEL_RW,
                &mut allocator
            ),
            Err(Error::OutOfRange)
        );
    }

    #[test]
    fn table_count_tracks_the_tree() {
        let (walker, mut allocator, root) = setup(64);
        walker
            .map_page(root, 0x1_0000, 0x8000_0000, Perm::KERNEL_RW, &mut allocator)
            .unwrap();
        // Root + L1 + L2 + L3.
        assert_eq!(walker.table_count(root), 4);
    }

    #[test]
    fn tcr_and_mair_values_match_the_contract() {
        let tcr = tcr_value(true, 5);
        assert_eq!(tcr & 0x3f, 16);
        assert_eq!((tcr >> 16) & 0x3f, 16);
        assert_eq!((tcr >> 14) & 0x3, 0);
        assert_eq!((tcr >> 30) & 0x3, 2);
        assert_eq!((tcr >> 32) & 0x7, 5);
        assert_ne!(tcr & (1 << 36), 0);
        assert_eq!((tcr >> 7) & 1, 0);
        assert_eq!((tcr >> 23) & 1, 0);
        assert_eq!(tcr_value(false, 2) & (1 << 36), 0);
        assert_eq!(asid_width(0), 8);
        assert_eq!(asid_width(1 << 4), 16);
    }
}
