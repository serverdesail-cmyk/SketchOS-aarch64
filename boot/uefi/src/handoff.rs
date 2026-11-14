//! Final boot-services teardown, bootstrap page tables and handoff.
//!
//! Ordering matters:
//!
//! 1. Allocate the boot-info record, the normalized memory-map array and the
//!    bootstrap page-table pool as `EfiLoaderData` pages. They must survive
//!    `ExitBootServices` and are described as loader data by the final map.
//! 2. Write the preliminary boot info.
//! 3. Call `ExitBootServices`, obtaining the authoritative final map.
//! 4. Copy the final map into the preallocated array.
//! 5. Build the two bootstrap translation roots:
//!    * TTBR0: an identity map covering the kernel image and the boot record,
//!      used by `boot.S` until it jumps into the higher half.
//!    * TTBR1: the higher-half kernel image plus the linear physical map.
//! 6. Patch the version-2 translation fields, clean everything to the point of
//!    coherency and branch to the physical kernel entry.
//!
//! No UEFI service, no allocation and no protocol is touched after step 3. The
//! table builder therefore writes through the firmware identity map that is
//! still active at that moment; the kernel installs the finished tables itself.

use core::{mem, ptr};

use crate::elf::{LoadedKernel, PF_W, PF_X};
use sketchos_bootinfo::{
    memory_kind, BootInfo, MemoryDescriptor, MemoryRegion, FLAG_HIGHER_HALF, FLAG_MEMORY_MAP,
    KERNEL_VA_BASE, PAGE_SIZE, PHYSMAP_BASE,
};
use sketchos_mm::pagetable::{self, Perm, TableAllocator, TableMemory, Walker};
use uefi::boot::{self, AllocateType, MemoryType};
use uefi::mem::memory_map::MemoryMap;

/// Extra descriptors reserved above the preliminary map length. The final map
/// can grow slightly while `ExitBootServices` allocates its own scratch
/// buffer, but not by hundreds of entries.
const DESCRIPTOR_SLACK: usize = 256;

/// Bootstrap page-table pool. A kernel map with 4 KiB edges, the physical map
/// with 1 GiB/2 MiB blocks and the RAM promotion fit comfortably here.
const TABLE_POOL_BYTES: usize = 8 * 1024 * 1024;
const TABLE_POOL_PAGES: usize = TABLE_POOL_BYTES / PAGE_SIZE as usize;

const MAX_PHYS: u64 = sketchos_bootinfo::MAX_PHYS;
const BLOCK_1G: u64 = 1024 * 1024 * 1024;

/// Physical table addresses are directly writable under the firmware identity
/// map that remains active until the kernel installs the new roots.
struct IdentityMemory;

impl TableMemory for IdentityMemory {
    fn table(&self, physical: u64) -> *mut u64 {
        physical as *mut u64
    }
}

/// Bump allocator over the preallocated `EfiLoaderData` pool.
struct Pool {
    base: u64,
    next: usize,
    pages: usize,
}

impl TableAllocator for Pool {
    fn alloc_table(&mut self) -> Option<u64> {
        if self.next >= self.pages {
            return None;
        }
        let table = self.base + self.next as u64 * PAGE_SIZE;
        self.next += 1;
        Some(table)
    }
}

/// Consume the loader state and enter the kernel. Never returns.
///
/// # Safety
///
/// Must be called exactly once, after every protocol, `Vec` and pool
/// allocation other than the handoff pages has been dropped.
pub unsafe fn enter(info: BootInfo, kernel: LoadedKernel) -> ! {
    let descriptor_size = mem::size_of::<MemoryDescriptor>();

    // Preliminary map only supplies a capacity estimate. It is freed while
    // Boot Services are still available.
    let preliminary = boot::memory_map(MemoryType::LOADER_DATA)
        .expect("failed to read the preliminary UEFI memory map");
    let capacity = preliminary
        .len()
        .checked_add(DESCRIPTOR_SLACK)
        .expect("UEFI memory map is implausibly large");
    drop(preliminary);

    // Boot info page.
    let info_ptr = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, 1)
        .expect("failed to allocate the boot-info page")
        .as_ptr()
        .cast::<BootInfo>();

    // Descriptor array. The array is written only after Boot Services exit.
    let map_bytes = capacity
        .checked_mul(descriptor_size)
        .expect("descriptor array size overflow");
    let map_pages = map_bytes.div_ceil(PAGE_SIZE as usize);
    // `map_range` maps whole pages, so its size must be page-rounded even
    // though the array itself is only `map_bytes` bytes long.
    let map_span = map_pages as u64 * PAGE_SIZE;
    let map_ptr = boot::allocate_pages(
        AllocateType::AnyPages,
        MemoryType::LOADER_DATA,
        map_pages,
    )
    .expect("failed to allocate the memory-map array")
    .as_ptr()
    .cast::<MemoryDescriptor>();

    // Bootstrap table pool. Its tables are written after Boot Services exit,
    // so they are loader data that survives the transition.
    let pool_base = boot::allocate_pages(
        AllocateType::AnyPages,
        MemoryType::LOADER_DATA,
        TABLE_POOL_PAGES,
    )
    .expect("failed to allocate the bootstrap page-table pool")
    .as_ptr() as u64;

    // The walker clears every child table it allocates, but the roots are
    // supplied by the caller. Clear the whole pool so firmware leftovers can
    // never become live descriptors.
    ptr::write_bytes(pool_base as *mut u8, 0, TABLE_POOL_BYTES);

    // Expose the array now; the count is patched after the final map is known.
    let mut boot_info = info;
    boot_info.memory_map = MemoryRegion {
        address: map_ptr as u64,
        count: 0,
        entry_size: descriptor_size as u32,
        _reserved: 0,
    };
    ptr::write(info_ptr, boot_info);

    // From here on the only UEFI call is the exit itself.
    let final_map = boot::exit_boot_services(Some(MemoryType::LOADER_DATA));

    // Copy the authoritative map. `MemoryMapOwned` is intentionally leaked:
    // dropping it would interact with the now-disabled boot-services
    // allocator, and the kernel does not need the raw UEFI buffer.
    let mut count = 0usize;
    let mut highest = (pool_base + TABLE_POOL_BYTES as u64)
        .max(kernel.end)
        .max(info_ptr as u64 + PAGE_SIZE)
        .max(map_ptr as u64 + map_span);
    for entry in final_map.entries() {
        if count == capacity {
            // Never hand the kernel a silently truncated memory map. A partial
            // map can make reserved RAM or MMIO look allocatable/unmapped.
            panic!("final UEFI memory map exceeds preallocated descriptor capacity");
        }

        let raw_kind = entry.ty.0;
        let kind = if raw_kind <= memory_kind::PERSISTENT {
            raw_kind
        } else {
            memory_kind::UNKNOWN
        };

        ptr::write(
            map_ptr.add(count),
            MemoryDescriptor {
                kind,
                _reserved: 0,
                physical_start: entry.phys_start,
                page_count: entry.page_count,
                attributes: entry.att.bits(),
            },
        );

        if let Some(end) = entry
            .phys_start
            .checked_add(entry.page_count.saturating_mul(PAGE_SIZE))
        {
            highest = highest.max(end);
        }
        count += 1;
    }
    mem::forget(final_map);

    (*info_ptr).memory_map.count = count as u64;
    (*info_ptr).flags |= FLAG_MEMORY_MAP;

    if kernel_phys_to_virt(kernel.start) != kernel.virt_start
        || kernel_phys_to_virt(kernel.end) != kernel.virt_end
    {
        panic!("kernel virtual range does not match its physical range");
    }

    // Include firmware-described devices the kernel will touch during boot.
    if boot_info.has_flag(sketchos_bootinfo::FLAG_UART) && boot_info.uart_base != 0 {
        highest = highest.max(boot_info.uart_base.saturating_add(PAGE_SIZE));
    }
    if boot_info.has_flag(sketchos_bootinfo::FLAG_FRAMEBUFFER) {
        let fb = boot_info.framebuffer;
        if let Some(end) = fb.address.checked_add(fb.size) {
            highest = highest.max(end);
        }
    }
    if highest == 0 {
        panic!("physical address space is empty");
    }
    let physmap_size = align_up(highest, BLOCK_1G).min(MAX_PHYS);
    if physmap_size == 0 || highest > physmap_size {
        panic!("physical address space exceeds the 512 GiB map");
    }

    let memory = IdentityMemory;
    let walker = Walker::new(memory);
    let mut pool = Pool {
        base: pool_base,
        next: 0,
        pages: TABLE_POOL_PAGES,
    };

    // --- TTBR0: bootstrap identity map ----------------------------------
    let ttbr0_root = pool
        .alloc_table()
        .expect("no table for the TTBR0 root");
    walker
        .map_range(
            ttbr0_root,
            kernel.start,
            kernel.start,
            kernel.end - kernel.start,
            Perm::BOOT_RWX,
            &mut pool,
        )
        .expect("failed to map the kernel in the bootstrap identity map");
    // `boot.S` reads the version-2 fields before it leaves the identity map.
    walker
        .map_range(
            ttbr0_root,
            info_ptr as u64,
            info_ptr as u64,
            PAGE_SIZE,
            Perm::BOOT_RWX,
            &mut pool,
        )
        .expect("failed to map the boot record in the bootstrap identity map");
    // The kernel clears .bss and uses its early stack, both inside the image.
    // The descriptor array is only read after the switch to the physical map.

    // --- TTBR1: higher half + linear physical map -----------------------
    let ttbr1_root = pool
        .alloc_table()
        .expect("no table for the TTBR1 root");
    walker
        .map_range(
            ttbr1_root,
            PHYSMAP_BASE,
            0,
            physmap_size,
            Perm::DEVICE,
            &mut pool,
        )
        .expect("failed to create the physical map");

    // Promote every RAM descriptor to Normal write-back memory.
    for index in 0..count {
        // SAFETY: `index < count` entries were just written above.
        let entry = ptr::read(map_ptr.add(index));
        if is_ram(entry.kind) {
            if let Some(end) = entry.end() {
                if entry.physical_start < end {
                    walker
                        .map_range(
                            ttbr1_root,
                            PHYSMAP_BASE + entry.physical_start,
                            entry.physical_start,
                            end - entry.physical_start,
                            Perm::KERNEL_RW,
                            &mut pool,
                        )
                        .expect("failed to promote RAM in the physical map");
                }
            }
        }
    }

    // The boot record and descriptor array are loader data, but a firmware
    // that classifies them differently still needs them readable as Normal.
    walker
        .map_range(
            ttbr1_root,
            kernel_phys_to_virt(info_ptr as u64),
            info_ptr as u64,
            PAGE_SIZE,
            Perm::KERNEL_RW,
            &mut pool,
        )
        .expect("failed to map the boot record");
    walker
        .map_range(
            ttbr1_root,
            kernel_phys_to_virt(map_ptr as u64),
            map_ptr as u64,
            map_span,
            Perm::KERNEL_RW,
            &mut pool,
        )
        .expect("failed to map the memory-map array");

    // Kernel image: writable NX as a baseline, then each PT_LOAD overrides.
    walker
        .map_range(
            ttbr1_root,
            kernel.virt_start,
            kernel.start,
            kernel.end - kernel.start,
            Perm::KERNEL_RW,
            &mut pool,
        )
        .expect("failed to map the kernel image");
    for segment in &kernel.segments[..kernel.segment_count] {
        if segment.memsz == 0 {
            continue;
        }
        let physical = kernel.start + (segment.vaddr - kernel.min_vaddr);
        let virtual_address = kernel_phys_to_virt(physical);
        let size = align_up(segment.memsz, PAGE_SIZE);
        let perm = segment_perm(segment.flags);
        walker
            .map_range(
                ttbr1_root,
                virtual_address,
                physical,
                size,
                perm,
                &mut pool,
            )
            .expect("failed to map a kernel segment");
    }

    if boot_info.has_flag(sketchos_bootinfo::FLAG_UART) && boot_info.uart_base != 0 {
        walker
            .map_range(
                ttbr1_root,
                PHYSMAP_BASE + boot_info.uart_base,
                boot_info.uart_base,
                PAGE_SIZE,
                Perm::DEVICE,
                &mut pool,
            )
            .expect("failed to map the debug UART");
    }
    if boot_info.has_flag(sketchos_bootinfo::FLAG_FRAMEBUFFER) {
        let fb = boot_info.framebuffer;
        if fb.address != 0 && fb.size != 0 {
            walker
                .map_range(
                    ttbr1_root,
                    PHYSMAP_BASE + fb.address,
                    fb.address,
                    align_up(fb.size, PAGE_SIZE),
                    Perm::NORMAL_NC,
                    &mut pool,
                )
                .expect("failed to map the framebuffer");
        }
    }

    // --- Translation-control state --------------------------------------
    let mmfr0: u64;
    // SAFETY: ID_AA64MMFR0_EL1 is readable at the current exception level.
    core::arch::asm!("mrs {}, ID_AA64MMFR0_EL1", out(reg) mmfr0, options(nomem, nostack));
    let parange = mmfr0 & 0xf;
    if parange < 4 {
        panic!("the physical address range is below 44 bits");
    }
    let ips = parange.min(5);
    let asid16 = pagetable::asid_width(mmfr0) == 16;
    let tcr = pagetable::tcr_value(asid16, ips);
    let mair = pagetable::MAIR_VALUE;

    (*info_ptr).ttbr0_boot_root = ttbr0_root;
    (*info_ptr).ttbr1_root = ttbr1_root;
    (*info_ptr).tcr = tcr;
    (*info_ptr).mair = mair;
    (*info_ptr).kernel_va_start = kernel.virt_start;
    (*info_ptr).kernel_va_end = kernel.virt_end;
    (*info_ptr).physmap_base = PHYSMAP_BASE;
    (*info_ptr).physmap_size = physmap_size;
    (*info_ptr).flags |= FLAG_HIGHER_HALF;

    // Make the tables and every bootstrap write visible to the kernel before
    // it installs them. Instruction-cache maintenance already happened when
    // the image was relocated.
    let pool_end = pool_base + pool.next as u64 * PAGE_SIZE;
    crate::elf::clean_data_cache(pool_base, pool_end);
    crate::elf::clean_data_cache(info_ptr as u64, info_ptr as u64 + PAGE_SIZE);
    crate::elf::clean_data_cache(map_ptr as u64, map_ptr as u64 + map_bytes as u64);
    core::arch::asm!("dsb sy", "isb", options(nostack));

    let entry: extern "C" fn(u64) -> ! = mem::transmute(kernel.entry);
    entry(info_ptr as u64)
}

const fn kernel_phys_to_virt(physical: u64) -> u64 {
    KERNEL_VA_BASE.wrapping_add(physical)
}

const fn segment_perm(flags: u32) -> Perm {
    if flags & PF_X != 0 {
        Perm::KERNEL_RX
    } else if flags & PF_W != 0 {
        Perm::KERNEL_RW
    } else {
        Perm::KERNEL_RO
    }
}

fn is_ram(kind: u32) -> bool {
    matches!(
        kind,
        memory_kind::CONVENTIONAL
            | memory_kind::LOADER_CODE
            | memory_kind::LOADER_DATA
            | memory_kind::BOOT_SERVICES_CODE
            | memory_kind::BOOT_SERVICES_DATA
            | memory_kind::RUNTIME_SERVICES_CODE
            | memory_kind::RUNTIME_SERVICES_DATA
            | memory_kind::ACPI_RECLAIM
            | memory_kind::ACPI_NVS
            | memory_kind::PERSISTENT
    )
}

fn align_up(value: u64, align: u64) -> u64 {
    if value == 0 {
        return 0;
    }
    (value + align - 1) & !(align - 1)
}
