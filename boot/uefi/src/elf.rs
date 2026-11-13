//! ELF64 AArch64 kernel image loading.
//!
//! The kernel is a position-independent `ET_DYN` image linked at virtual
//! address zero. The loader chooses a free physical range below 4 GiB, copies
//! the image there, applies the `R_AARCH64_RELATIVE` relocations from the
//! dynamic segment, and returns both the physical and the higher-half virtual
//! entry point.
//!
//! Since milestone 0.0.4 physical and virtual addresses differ:
//!
//! * the image is copied to a physical base chosen by the loader,
//! * relocation *targets* are written at the physical address,
//! * relocation *values* point at the high virtual address
//!   (`KERNEL_VA_BASE + physical`).
//!
//! `ET_EXEC` images are still accepted for compatibility. They keep their
//! linked physical address and are mapped at `KERNEL_VA_BASE + p_paddr`.

use core::ptr;

use sketchos_bootinfo::KERNEL_VA_BASE;
use uefi::boot::{self, AllocateType, MemoryType};

const ELF_HEADER_SIZE: usize = 64;
const PROGRAM_HEADER_SIZE: usize = 56;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_AARCH64: u16 = 183;
const MAX_KERNEL_SPAN: u64 = 128 * 1024 * 1024;
const MAX_SEGMENTS: usize = 64;

/// ELF program-header permission bits. Read permission is implicit: every
/// mapped page is readable, and the loader chooses read-only when neither
/// write nor execute is requested.
pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;

/// The kernel is loaded below 4 GiB. The low physical range is the most
/// conservative choice for early boot and keeps every address representable
/// in 32 bits for diagnostics and future firmware interactions.
const ALLOCATION_MAX: u64 = 0xffff_ffff;

const DT_NULL: i64 = 0;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_RELAENT: i64 = 9;
const DT_REL: i64 = 17;
const DT_JMPREL: i64 = 23;

const R_AARCH64_NONE: u32 = 0;
const R_AARCH64_RELATIVE: u32 = 1027;

/// One `PT_LOAD` segment as the page-table builder needs it.
#[derive(Clone, Copy)]
pub struct LoadedSegment {
    /// Link-time virtual address (an offset from the image base).
    pub vaddr: u64,
    /// Memory size, not rounded to a page.
    pub memsz: u64,
    /// `PF_X`/`PF_W` bits.
    pub flags: u32,
}

impl LoadedSegment {
    pub const EMPTY: Self = Self {
        vaddr: 0,
        memsz: 0,
        flags: 0,
    };
}

#[derive(Clone, Copy)]
pub struct LoadedKernel {
    /// Physical entry point installed in `x0`/`elr` by the loader.
    pub entry: u64,
    /// Higher-half virtual entry point.
    pub entry_virt: u64,
    /// Physical image range, inclusive start / exclusive end.
    pub start: u64,
    pub end: u64,
    /// Higher-half virtual image range.
    pub virt_start: u64,
    pub virt_end: u64,
    /// Link-time start of the image (the base of the first segment).
    pub min_vaddr: u64,
    pub file_size: usize,
    pub segment_count: usize,
    pub relocation_count: usize,
    pub relocatable: bool,
    pub segments: [LoadedSegment; MAX_SEGMENTS],
}

#[derive(Clone, Copy)]
pub enum ElfError {
    TooSmall,
    BadMagic,
    NotElf64,
    NotLittleEndian,
    NotAarch64,
    NotExecutable,
    BadProgramHeaders,
    BadSegment,
    AddressMismatch,
    EntryOutOfRange,
    TooLarge,
    AllocationFailed,
    BadDynamic,
    BadRelocation,
    WritableExecutable,
}

impl ElfError {
    pub fn message(self) -> &'static str {
        match self {
            Self::TooSmall => "file is smaller than an ELF64 header",
            Self::BadMagic => "bad ELF magic",
            Self::NotElf64 => "not an ELF64 image",
            Self::NotLittleEndian => "not a little-endian ELF image",
            Self::NotAarch64 => "not an AArch64 ELF image",
            Self::NotExecutable => "not an ET_EXEC or ET_DYN image",
            Self::BadProgramHeaders => "invalid program-header table",
            Self::BadSegment => "invalid PT_LOAD segment",
            Self::AddressMismatch => "ET_EXEC segment physical and virtual addresses differ",
            Self::EntryOutOfRange => "entry point is outside the loaded image",
            Self::TooLarge => "kernel image exceeds the supported size",
            Self::AllocationFailed => "could not reserve the kernel physical range",
            Self::BadDynamic => "invalid dynamic section",
            Self::BadRelocation => "unsupported or invalid relocation",
            Self::WritableExecutable => "kernel segment is both writable and executable",
        }
    }
}

#[derive(Clone, Copy)]
struct Segment {
    file_offset: usize,
    vaddr: u64,
    filesz: usize,
    memsz: usize,
    flags: u32,
}

pub fn load(file: &[u8]) -> Result<LoadedKernel, ElfError> {
    if file.len() < ELF_HEADER_SIZE {
        return Err(ElfError::TooSmall);
    }
    if &file[0..4] != b"\x7fELF" {
        return Err(ElfError::BadMagic);
    }
    if file[4] != 2 {
        return Err(ElfError::NotElf64);
    }
    if file[5] != 1 {
        return Err(ElfError::NotLittleEndian);
    }

    let e_type = read_u16(file, 16)?;
    let e_machine = read_u16(file, 18)?;
    let entry = read_u64(file, 24)?;
    let phoff = read_u64(file, 32)?;
    let phentsize = read_u16(file, 54)? as usize;
    let phnum = read_u16(file, 56)? as usize;

    if e_machine != EM_AARCH64 {
        return Err(ElfError::NotAarch64);
    }
    if e_type != ET_EXEC && e_type != ET_DYN {
        return Err(ElfError::NotExecutable);
    }
    let relocatable = e_type == ET_DYN;
    if phnum == 0 || phnum > MAX_SEGMENTS || phentsize < PROGRAM_HEADER_SIZE {
        return Err(ElfError::BadProgramHeaders);
    }
    let table_end = (phoff as usize)
        .checked_add(phnum.checked_mul(phentsize).ok_or(ElfError::BadProgramHeaders)?)
        .ok_or(ElfError::BadProgramHeaders)?;
    if table_end > file.len() {
        return Err(ElfError::BadProgramHeaders);
    }

    // First pass: validate segments and find the virtual extent. The program
    // headers are kept in a fixed array; `MAX_SEGMENTS` is small and this is
    // the loader, not the kernel.
    let mut segments = [Segment {
        file_offset: 0,
        vaddr: 0,
        filesz: 0,
        memsz: 0,
        flags: 0,
    }; MAX_SEGMENTS];
    let mut loaded = [LoadedSegment::EMPTY; MAX_SEGMENTS];
    let mut load_count = 0usize;
    let mut min_vaddr = u64::MAX;
    let mut max_vaddr = 0u64;
    let mut dynamic: Option<(usize, usize, u64)> = None;

    for index in 0..phnum {
        let ph = phoff as usize + index * phentsize;
        let p_type = read_u32(file, ph)?;
        let flags = read_u32(file, ph + 4)?;
        let offset = read_u64(file, ph + 8)?;
        let vaddr = read_u64(file, ph + 16)?;
        let paddr = read_u64(file, ph + 24)?;
        let filesz = read_u64(file, ph + 32)?;
        let memsz = read_u64(file, ph + 40)?;

        if p_type == PT_DYNAMIC {
            let offset_end = offset.checked_add(filesz).ok_or(ElfError::BadDynamic)?;
            if offset_end > file.len() as u64 {
                return Err(ElfError::BadDynamic);
            }
            dynamic = Some((offset as usize, filesz as usize, vaddr));
            continue;
        }
        if p_type != PT_LOAD {
            continue;
        }

        if filesz > memsz || memsz == 0 {
            return Err(ElfError::BadSegment);
        }
        // W^X is a hard invariant of the kernel image: a segment that is both
        // writable and executable would violate the final map's policy.
        if flags & PF_W != 0 && flags & PF_X != 0 {
            return Err(ElfError::WritableExecutable);
        }
        if !relocatable && vaddr != paddr {
            return Err(ElfError::AddressMismatch);
        }
        // The page-table builder maps whole pages; every segment must start on
        // a page boundary. The kernel linker aligns every section to 4 KiB.
        if vaddr % 4096 != 0 {
            return Err(ElfError::BadSegment);
        }
        let offset_end = offset.checked_add(filesz).ok_or(ElfError::BadSegment)?;
        if offset_end > file.len() as u64 {
            return Err(ElfError::BadSegment);
        }
        let segment_end = vaddr.checked_add(memsz).ok_or(ElfError::BadSegment)?;
        if segment_end <= vaddr {
            return Err(ElfError::BadSegment);
        }

        if load_count >= MAX_SEGMENTS {
            return Err(ElfError::BadProgramHeaders);
        }
        segments[load_count] = Segment {
            file_offset: offset as usize,
            vaddr,
            filesz: filesz as usize,
            memsz: memsz as usize,
            flags,
        };
        loaded[load_count] = LoadedSegment {
            vaddr,
            memsz,
            flags,
        };
        load_count += 1;

        min_vaddr = min_vaddr.min(vaddr);
        max_vaddr = max_vaddr.max(segment_end);
    }

    if load_count == 0 || min_vaddr == u64::MAX {
        return Err(ElfError::BadProgramHeaders);
    }
    // The load bias must be page-aligned so segment copies and relative
    // relocations land on their natural alignment.
    if min_vaddr % 4096 != 0 {
        return Err(ElfError::BadSegment);
    }
    let span = max_vaddr - min_vaddr;
    if span > MAX_KERNEL_SPAN {
        return Err(ElfError::TooLarge);
    }
    if entry < min_vaddr || entry >= max_vaddr {
        return Err(ElfError::EntryOutOfRange);
    }
    if relocatable && dynamic.is_none() {
        return Err(ElfError::BadDynamic);
    }

    let pages = span.div_ceil(4096);

    // Reserve the image. A relocatable image is placed at the highest free
    // range below 4 GiB; a fixed image keeps its linked address.
    let (base, phys_bias, virt_bias) = if relocatable {
        let allocation = boot::allocate_pages(
            AllocateType::MaxAddress(ALLOCATION_MAX),
            MemoryType::LOADER_DATA,
            pages as usize,
        )
        .map_err(|_| ElfError::AllocationFailed)?;
        let base = allocation.as_ptr() as u64;
        if base == 0 || base % 4096 != 0 {
            return Err(ElfError::AllocationFailed);
        }
        let end = base
            .checked_add(pages * 4096)
            .ok_or(ElfError::AllocationFailed)?;
        if end == 0 || end - 1 > ALLOCATION_MAX {
            return Err(ElfError::AllocationFailed);
        }
        (base, base - min_vaddr, KERNEL_VA_BASE + (base - min_vaddr))
    } else {
        let allocation = boot::allocate_pages(
            AllocateType::Address(min_vaddr),
            MemoryType::LOADER_DATA,
            pages as usize,
        )
        .map_err(|_| ElfError::AllocationFailed)?;
        if allocation.as_ptr() as u64 != min_vaddr {
            return Err(ElfError::AllocationFailed);
        }
        (min_vaddr, 0u64, KERNEL_VA_BASE)
    };

    // Zero the full span, including inter-segment gaps and .bss.
    unsafe {
        ptr::write_bytes(base as *mut u8, 0, (pages * 4096) as usize);
    }

    if relocatable {
        uefi::println!(
            "Kernel allocation: {} pages at 0x{:016x} (relocatable, phys bias 0x{:016x}, virt bias 0x{:016x})",
            pages,
            base,
            phys_bias,
            virt_bias
        );
    } else {
        uefi::println!(
            "Kernel allocation: {} pages at 0x{:016x} (fixed, virt bias 0x{:016x})",
            pages,
            base,
            virt_bias
        );
    }

    // Second pass: copy file-backed bytes and zero .bss tails. The destination
    // is `phys_bias + vaddr`, which is `base + (vaddr - min_vaddr)`.
    for segment in &segments[..load_count] {
        let dest = (phys_bias + segment.vaddr) as *mut u8;
        unsafe {
            if segment.filesz != 0 {
                ptr::copy_nonoverlapping(
                    file.as_ptr().add(segment.file_offset),
                    dest,
                    segment.filesz,
                );
            }
            if segment.memsz > segment.filesz {
                ptr::write_bytes(dest.add(segment.filesz), 0, segment.memsz - segment.filesz);
            }
        }

        uefi::println!(
            "  PT_LOAD phys 0x{:016x} virt 0x{:016x} flags 0x{:x} filesz {:>8} memsz {:>8}",
            phys_bias + segment.vaddr,
            virt_bias + segment.vaddr,
            segment.flags,
            segment.filesz,
            segment.memsz
        );
    }

    let relocation_count = if relocatable {
        let (dyn_offset, dyn_size, _) = dynamic.ok_or(ElfError::BadDynamic)?;
        apply_relocations(
            file,
            dyn_offset,
            dyn_size,
            &segments[..load_count],
            min_vaddr,
            max_vaddr,
            min_vaddr + pages * 4096,
            phys_bias,
            virt_bias,
        )?
    } else {
        0
    };

    unsafe {
        synchronize_instruction_cache(base, base + pages * 4096);
    }

    let phys_entry = phys_bias
        .checked_add(entry)
        .ok_or(ElfError::EntryOutOfRange)?;
    let virt_entry = virt_bias
        .checked_add(entry)
        .ok_or(ElfError::EntryOutOfRange)?;

    Ok(LoadedKernel {
        entry: phys_entry,
        entry_virt: virt_entry,
        start: base,
        end: base + pages * 4096,
        virt_start: virt_bias + min_vaddr,
        // The mapping covers whole pages, so the virtual range ends where the
        // page-rounded image ends, not at the last segment's `memsz`.
        virt_end: virt_bias + pages * 4096,
        min_vaddr,
        file_size: file.len(),
        segment_count: load_count,
        relocation_count,
        relocatable,
        segments: loaded,
    })
}

/// Apply the `R_AARCH64_RELATIVE` relocations described by the dynamic
/// segment. The table's virtual address is translated to a file offset through
/// the `PT_LOAD` mapping, so no section headers are required.
///
/// `phys_bias` converts a link-time address to its physical destination;
/// `virt_bias` converts it to the high virtual value stored by a relocation.
/// `value_limit` is the end of the page-rounded image span: an addend may name
/// the one-past-the-end symbols the linker emits (for example `__kernel_end`)
/// even though they lie beyond the last segment's `memsz`.
#[allow(clippy::too_many_arguments)]
fn apply_relocations(
    file: &[u8],
    dyn_offset: usize,
    dyn_size: usize,
    segments: &[Segment],
    min_vaddr: u64,
    max_vaddr: u64,
    value_limit: u64,
    phys_bias: u64,
    virt_bias: u64,
) -> Result<usize, ElfError> {
    let mut rela_vaddr = None;
    let mut rela_size = 0u64;
    let mut rela_ent = 24u64;

    let mut offset = dyn_offset;
    let end = dyn_offset.checked_add(dyn_size).ok_or(ElfError::BadDynamic)?;
    while offset + 16 <= end {
        let tag = read_i64(file, offset)?;
        let value = read_u64(file, offset + 8)?;
        match tag {
            DT_NULL => break,
            DT_RELA => rela_vaddr = Some(value),
            DT_RELASZ => rela_size = value,
            DT_RELAENT => rela_ent = value,
            // Old-style REL relocations and PLT relocations need a dynamic
            // symbol resolver. A static kernel image must not contain them.
            DT_REL | DT_JMPREL => return Err(ElfError::BadRelocation),
            _ => {}
        }
        offset += 16;
    }

    let rela_vaddr = match rela_vaddr {
        Some(address) => address,
        None => return Ok(0),
    };
    if rela_size == 0 {
        return Ok(0);
    }
    if rela_ent != 24 {
        return Err(ElfError::BadRelocation);
    }

    let table_end = rela_vaddr
        .checked_add(rela_size)
        .ok_or(ElfError::BadRelocation)?;
    let file_offset = segments
        .iter()
        .find_map(|segment| {
            let segment_end = segment.vaddr + segment.filesz as u64;
            if rela_vaddr >= segment.vaddr && table_end <= segment_end {
                Some(segment.file_offset as u64 + (rela_vaddr - segment.vaddr))
            } else {
                None
            }
        })
        .ok_or(ElfError::BadRelocation)? as usize;

    let count = (rela_size / rela_ent) as usize;
    for index in 0..count {
        let offset = file_offset + index * 24;
        let r_offset = read_u64(file, offset)?;
        let r_info = read_u64(file, offset + 8)?;
        let r_addend = read_i64(file, offset + 16)?;
        let r_type = (r_info & 0xffff_ffff) as u32;

        match r_type {
            R_AARCH64_NONE => continue,
            R_AARCH64_RELATIVE => {
                if r_offset < min_vaddr
                    || r_offset.checked_add(8).ok_or(ElfError::BadRelocation)? > max_vaddr
                    || r_offset % 8 != 0
                {
                    return Err(ElfError::BadRelocation);
                }
                if r_addend < min_vaddr as i64 || r_addend > value_limit as i64 {
                    return Err(ElfError::BadRelocation);
                }
                // The write happens at the physical address; the stored value
                // is the high virtual address the kernel will use.
                let target = (phys_bias + r_offset) as *mut u64;
                let value = virt_bias.wrapping_add(r_addend as u64);
                // SAFETY: `r_offset` was checked against the loaded span and
                // the destination is inside the allocation made above.
                unsafe { ptr::write(target, value) };
            }
            _ => return Err(ElfError::BadRelocation),
        }
    }

    Ok(count)
}

/// Clean dirty data-cache lines over `[start, end)` to the point of
/// unification, then invalidate the instruction cache. Required on real
/// hardware after writing executable code with the MMU/caches enabled.
pub unsafe fn synchronize_instruction_cache(start: u64, end: u64) {
    clean_data_cache(start, end);
    core::arch::asm!(
        "dsb ish",
        "ic iallu",
        "dsb ish",
        "isb",
        options(nostack)
    );
}

/// Clean dirty data-cache lines over `[start, end)` to the point of
/// coherency. Used for page tables and handoff records written after
/// `ExitBootServices`.
///
/// # Safety
///
/// `[start, end)` must be a valid physical range the loader owns.
pub unsafe fn clean_data_cache(start: u64, end: u64) {
    use core::arch::asm;

    if start >= end {
        return;
    }

    // Data cache line size in bytes: 4 << CTR_EL0.DminLine.
    let ctr: u64;
    asm!("mrs {}, ctr_el0", out(reg) ctr, options(nostack));
    let line = 4u64 << ((ctr >> 16) & 0xf);

    let mut address = start & !(line - 1);
    while address < end {
        asm!("dc civac, {}", in(reg) address, options(nostack));
        address = address.saturating_add(line);
    }

    asm!("dsb ish", options(nostack));
}

fn read_u16(file: &[u8], offset: usize) -> Result<u16, ElfError> {
    let bytes = file
        .get(offset..offset + 2)
        .ok_or(ElfError::BadProgramHeaders)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(file: &[u8], offset: usize) -> Result<u32, ElfError> {
    let bytes = file
        .get(offset..offset + 4)
        .ok_or(ElfError::BadProgramHeaders)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64(file: &[u8], offset: usize) -> Result<u64, ElfError> {
    let bytes = file
        .get(offset..offset + 8)
        .ok_or(ElfError::BadProgramHeaders)?;
    Ok(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

fn read_i64(file: &[u8], offset: usize) -> Result<i64, ElfError> {
    Ok(read_u64(file, offset)? as i64)
}
