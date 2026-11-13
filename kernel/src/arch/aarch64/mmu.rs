//! Kernel-owned AArch64 stage-1 page tables.
//!
//! Since milestone 0.0.4 the loader enters the kernel in the higher half:
//!
//! * the kernel image is mapped at `KERNEL_VA_BASE + physical`,
//! * every physical byte is reachable at `PHYSMAP_BASE + physical`,
//! * a bootstrap identity root stays in TTBR0 until the kernel seals it.
//!
//! This module rebuilds the final map from scratch with the shared
//! [`sketchos_mm::pagetable`] walker. The loader's tables are only a bootstrap
//! aid; the final policy is enforced here:
//!
//! * `.text` is read-only and executable, `.rodata` is read-only, and
//!   `.data`/`.bss`/stacks are writable and non-executable (W^X),
//! * the physical map defaults to Device-nGnRnE and promotes RAM to Normal
//!   write-back memory,
//! * the framebuffer is Normal non-cacheable so pixel writes bypass the cache.
//!
//! All tables come from a fixed 8 MiB BSS pool. If a machine needs more tables
//! than the pool holds, [`init`] fails and the kernel keeps running under the
//! loader map instead of corrupting memory.

#![allow(unsafe_op_in_unsafe_fn)]

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

use sketchos_bootinfo::{
    memory_kind, phys_to_virt, BootInfo, MemoryDescriptor, FLAG_FRAMEBUFFER, FLAG_UART,
    KERNEL_VA_BASE, MAX_PHYS, PAGE_SIZE, PHYSMAP_BASE,
};
use sketchos_mm::pagetable::{
    self, Perm, TableAllocator, TableMemory, Translation, Walker, AF, PXN, UXN,
};

use crate::acpi::Platform;
use crate::println;

const TABLE_ENTRIES: usize = pagetable::TABLE_ENTRIES;
const BLOCK_1G: u64 = pagetable::BLOCK_1G;
const BLOCK_2M: u64 = pagetable::BLOCK_2M;
/// 8 MiB of bootstrap/final page tables: 2048 four-KiB tables.
const TABLE_POOL_BYTES: usize = 8 * 1024 * 1024;
const TABLE_COUNT: usize = TABLE_POOL_BYTES / PAGE_SIZE as usize;

#[repr(C, align(4096))]
#[derive(Clone, Copy)]
struct PageTable([u64; TABLE_ENTRIES]);

static mut POOL: [PageTable; TABLE_COUNT] = [PageTable([0; TABLE_ENTRIES]); TABLE_COUNT];
static mut POOL_NEXT: usize = 0;

static mut ENABLED: bool = false;
static mut ROOT: u64 = 0;
static mut BOOT_ROOT: u64 = 0;
static mut PHYS_SIZE: u64 = 0;
static mut TABLE_USED: usize = 0;
static mut ASID16: bool = false;

extern "C" {
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __data_start: u8;
    static __kernel_end: u8;
}

/// Physical table addresses reach the kernel through the linear map.
struct PhysmapMemory;

impl TableMemory for PhysmapMemory {
    fn table(&self, physical: u64) -> *mut u64 {
        phys_to_virt(physical) as *mut u64
    }
}

/// Bump allocator over the fixed BSS pool. The walker zeroes every table it
/// receives, so this only has to hand out page-aligned physical addresses.
struct PoolAllocator;

impl TableAllocator for PoolAllocator {
    fn alloc_table(&mut self) -> Option<u64> {
        unsafe {
            if POOL_NEXT >= TABLE_COUNT {
                return None;
            }
            let index = POOL_NEXT;
            POOL_NEXT += 1;
            let table = (core::ptr::addr_of_mut!(POOL) as *mut PageTable).add(index);
            let virtual_address = table as u64;
            if virtual_address < KERNEL_VA_BASE {
                return None;
            }
            Some(virtual_address - KERNEL_VA_BASE)
        }
    }
}

/// The translation-control state a secondary core must install before it can
/// run Rust code with the MMU on.
#[derive(Clone, Copy, Debug)]
pub struct MmuState {
    /// Bootstrap identity root. A secondary starts on a physical stack, so
    /// TTBR0 must stay usable until it has moved to the higher-half stack.
    pub ttbr0: u64,
    pub ttbr1: u64,
    pub tcr: u64,
    pub mair: u64,
    pub sctlr: u64,
    pub cpacr: u64,
    pub vbar: u64,
}

/// Read the live EL1 translation state. Valid only after [`init`] succeeded.
pub fn state() -> MmuState {
    let (ttbr0, ttbr1, tcr, mair, sctlr, cpacr, vbar): (u64, u64, u64, u64, u64, u64, u64);
    // SAFETY: all seven registers are readable at EL1.
    unsafe {
        asm!("mrs {}, TTBR0_EL1", out(reg) ttbr0, options(nomem, nostack));
        asm!("mrs {}, TTBR1_EL1", out(reg) ttbr1, options(nomem, nostack));
        asm!("mrs {}, TCR_EL1", out(reg) tcr, options(nomem, nostack));
        asm!("mrs {}, MAIR_EL1", out(reg) mair, options(nomem, nostack));
        asm!("mrs {}, SCTLR_EL1", out(reg) sctlr, options(nomem, nostack));
        asm!("mrs {}, CPACR_EL1", out(reg) cpacr, options(nomem, nostack));
        asm!("mrs {}, VBAR_EL1", out(reg) vbar, options(nomem, nostack));
    }
    MmuState {
        ttbr0,
        ttbr1,
        tcr,
        mair,
        sctlr,
        cpacr,
        vbar,
    }
}

/// True when the kernel replaced the loader's higher-half tables.
pub fn is_enabled() -> bool {
    // SAFETY: a single boot CPU reads this flag; no concurrent writer exists.
    unsafe { ENABLED }
}

/// Physical address of the installed TTBR1 root, or zero.
pub fn root() -> u64 {
    // SAFETY: see `is_enabled`.
    unsafe { ROOT }
}

/// Build and install the final higher-half tables.
pub fn init(info: &BootInfo, platform: Option<&Platform>) -> Result<(), &'static str> {
    // SAFETY: called once on the boot CPU before the scheduler exists.
    unsafe {
        if ENABLED {
            return Ok(());
        }
        if crate::arch::aarch64::current_el() != 1 {
            return Err("stage-1 setup requires EL1");
        }
        if info.ttbr1_root == 0 || info.ttbr0_boot_root == 0 {
            return Err("loader did not describe a translation root");
        }

        POOL_NEXT = 0;
        let mmfr0: u64;
        asm!("mrs {}, ID_AA64MMFR0_EL1", out(reg) mmfr0, options(nomem, nostack));
        ASID16 = pagetable::asid_width(mmfr0) == 16;

        let walker = Walker::new(PhysmapMemory);
        let mut allocator = PoolAllocator;
        let root = allocator
            .alloc_table()
            .ok_or("out of page tables for the kernel root")?;

        // The physical map defaults to Device-nGnRnE over the entire 512 GiB
        // window. A descriptor that firmware forgot to list is therefore
        // non-cacheable rather than wrongly cacheable. 512 one-GiB blocks fit
        // in one L1 table.
        walker
            .map_range(root, PHYSMAP_BASE, 0, MAX_PHYS, Perm::DEVICE, &mut allocator)
            .map_err(|_| "failed to build the physical map")?;

        // Promote every RAM descriptor to Normal write-back memory.
        let map_bytes = info
            .memory_map
            .count
            .checked_mul(info.memory_map.entry_size as u64)
            .ok_or("memory-map size overflow")?;
        // `map_range` works in whole pages; the array itself is only
        // descriptor-aligned.
        let map_span = round_up(map_bytes, PAGE_SIZE);
        for index in 0..info.memory_map.count as usize {
            let entry = descriptor(info, index)?;
            if is_ram(entry.kind) {
                if let Some(end) = entry.end() {
                    if entry.physical_start < end && end <= MAX_PHYS {
                        walker
                            .map_range(
                                root,
                                PHYSMAP_BASE + entry.physical_start,
                                entry.physical_start,
                                end - entry.physical_start,
                                Perm::KERNEL_RW,
                                &mut allocator,
                            )
                            .map_err(|_| "failed to promote RAM")?;
                    }
                }
            }
        }

        // Kernel sections. The loader's bootstrap map used PT_LOAD flags; the
        // final map refines them to the linker's section boundaries.
        map_symbol_range(
            &walker,
            root,
            &mut allocator,
            &__text_start,
            &__text_end,
            Perm::KERNEL_RX,
        )?;
        map_symbol_range(
            &walker,
            root,
            &mut allocator,
            &__rodata_start,
            &__rodata_end,
            Perm::KERNEL_RO,
        )?;
        map_symbol_range(
            &walker,
            root,
            &mut allocator,
            &__data_start,
            &__kernel_end,
            Perm::KERNEL_RW,
        )?;

        // Loader records and the memory map are loader data in the final map,
        // but map them explicitly so a firmware that classifies them
        // differently cannot leave them Device or unmapped.
        walker
            .map_range(
                root,
                PHYSMAP_BASE + info.memory_map.address,
                info.memory_map.address,
                map_span,
                Perm::KERNEL_RW,
                &mut allocator,
            )
            .map_err(|_| "failed to map the memory-map array")?;

        if info.has_flag(FLAG_UART) && info.uart_base != 0 {
            walker
                .map_range(
                    root,
                    PHYSMAP_BASE + info.uart_base,
                    info.uart_base,
                    PAGE_SIZE,
                    Perm::DEVICE,
                    &mut allocator,
                )
                .map_err(|_| "failed to map the debug UART")?;
        }

        if info.has_flag(FLAG_FRAMEBUFFER) {
            let fb = info.framebuffer;
            if fb.address != 0 && fb.size != 0 {
                walker
                    .map_range(
                        root,
                        PHYSMAP_BASE + fb.address,
                        fb.address,
                        round_up(fb.size, PAGE_SIZE),
                        Perm::NORMAL_NC,
                        &mut allocator,
                    )
                    .map_err(|_| "failed to map the framebuffer")?;
            }
        }

        if let Some(platform) = platform {
            map_platform_devices(&walker, root, &mut allocator, platform)?;
        }

        // Install the final TTBR1 root. TTBR0 stays the loader's identity root
        // so secondary cores can start on a physical stack. TCR/MAIR keep the
        // values the loader's tables were built for.
        asm!("msr TTBR1_EL1, {}", in(reg) root, options(nomem, nostack));
        asm!("msr TCR_EL1, {}", in(reg) info.tcr, options(nomem, nostack));
        asm!("msr MAIR_EL1, {}", in(reg) info.mair, options(nomem, nostack));
        asm!("dsb ish", "isb", options(nomem, nostack));
        asm!("tlbi vmalle1", "dsb ish", "isb", options(nomem, nostack));

        TABLE_USED = POOL_NEXT;
        ROOT = root;
        BOOT_ROOT = info.ttbr0_boot_root;
        PHYS_SIZE = info.physmap_size;
        ENABLED = true;
        Ok(())
    }
}

fn descriptor(info: &BootInfo, index: usize) -> Result<MemoryDescriptor, &'static str> {
    let address = info
        .memory_map
        .address
        .checked_add(index as u64 * info.memory_map.entry_size as u64)
        .ok_or("memory-map address overflow")?;
    // SAFETY: the loader validated every descriptor before handoff; the array
    // is inside the linear map.
    Ok(unsafe { core::ptr::read_volatile(phys_to_virt(address) as *const MemoryDescriptor) })
}

fn map_symbol_range(
    walker: &Walker<PhysmapMemory>,
    root: u64,
    allocator: &mut PoolAllocator,
    start: &u8,
    end: &u8,
    perm: Perm,
) -> Result<(), &'static str> {
    let virtual_start = start as *const u8 as u64;
    let virtual_end = end as *const u8 as u64;
    if virtual_start == 0 || virtual_end < virtual_start {
        return Err("invalid kernel section symbol");
    }
    if virtual_end == virtual_start {
        return Ok(());
    }
    let physical = virtual_start
        .checked_sub(KERNEL_VA_BASE)
        .ok_or("kernel section is outside the higher half")?;
    walker
        .map_range(
            root,
            virtual_start,
            physical,
            round_up(virtual_end - virtual_start, PAGE_SIZE),
            perm,
            allocator,
        )
        .map_err(|_| "failed to map a kernel section")
}

fn map_platform_devices(
    walker: &Walker<PhysmapMemory>,
    root: u64,
    allocator: &mut PoolAllocator,
    platform: &Platform,
) -> Result<(), &'static str> {
    let mut map = |base: u64, length: u64| -> Result<(), &'static str> {
        if base == 0 || length == 0 {
            return Ok(());
        }
        let Some(end) = base.checked_add(length) else {
            return Err("MMIO range overflows");
        };
        if end > MAX_PHYS {
            return Err("MMIO range exceeds the 512 GiB map");
        }
        walker
            .map_range(
                root,
                PHYSMAP_BASE + base,
                base,
                round_up(length, PAGE_SIZE),
                Perm::DEVICE,
                allocator,
            )
            .map_err(|_| "failed to map a platform device")
    };

    if let Some(gicd) = platform.gicd {
        map(gicd.base, 0x10000)?;
    }
    for gicr in platform.gicr[..platform.gicr_count].iter() {
        map(gicr.base, (gicr.length as u64).max(0x20000))?;
    }
    for its in platform.its[..platform.its_count].iter() {
        map(its.base, 0x20000)?;
    }
    if let Some(spcr) = &platform.spcr {
        map(spcr.base, PAGE_SIZE)?;
    }
    for port in platform.dbg2[..platform.dbg2_count].iter() {
        map(port.base, PAGE_SIZE)?;
    }
    for window in platform.mcfg[..platform.mcfg_count].iter() {
        if window.end_bus < window.start_bus {
            continue;
        }
        let buses = (window.end_bus as u64 - window.start_bus as u64) + 1;
        let length = buses
            .checked_mul(1024 * 1024)
            .ok_or("MCFG window size overflow")?;
        map(window.base, length)?;
    }
    if let Some(tpm) = platform.tpm2 {
        map(tpm.control_area, PAGE_SIZE)?;
    }
    Ok(())
}

/// Print the final translation state and the W^X audit result.
pub fn report() {
    if !is_enabled() {
        println!("MMU: loader higher-half map still active");
        return;
    }
    // SAFETY: the counters are written only during `init`.
    let used = unsafe { TABLE_USED };
    let phys = unsafe { PHYS_SIZE };
    let boot = unsafe { BOOT_ROOT };
    println!(
        "MMU: higher half, TTBR1 0x{:016x}, TTBR0 boot 0x{:016x}, {} tables of {}, physmap {} GiB",
        root(),
        boot,
        used,
        TABLE_COUNT,
        phys / BLOCK_1G
    );
    match audit_wx() {
        Ok(()) => println!("MMU: W^X audit clean (no writable-executable leaf)"),
        Err(count) => println!("MMU: W^X audit found {} violations", count),
    }
}

/// Count leaf mappings that are both writable and executable at the privilege
/// level that owns them. The final map must contain none.
pub fn audit_wx() -> Result<(), u64> {
    if !is_enabled() {
        return Ok(());
    }
    let walker = Walker::new(PhysmapMemory);
    let memory = walker.memory();
    let mut violations = 0u64;
    // SAFETY: `root()` is a valid table tree built above.
    unsafe { audit_table(memory, root(), 0, &mut violations) };
    if violations == 0 {
        Ok(())
    } else {
        Err(violations)
    }
}

unsafe fn audit_table(
    memory: &PhysmapMemory,
    table: u64,
    level: u8,
    violations: &mut u64,
) {
    for index in 0..TABLE_ENTRIES {
        let descriptor = core::ptr::read_volatile(memory.table(table).add(index));
        if descriptor & 1 == 0 {
            continue;
        }
        let kind = descriptor & 0b11;
        if level < 3 && kind == 0b11 {
            audit_table(memory, descriptor & pagetable::ADDRESS_MASK, level + 1, violations);
            continue;
        }
        if level < 3 && kind != 0b01 {
            continue;
        }
        // AP[2] (bit 7) set means read-only; AP[1] (bit 6) set means EL0 access.
        let write = descriptor & (1 << 7) == 0;
        let user = descriptor & (1 << 6) != 0;
        let execute = if user {
            descriptor & UXN == 0
        } else {
            descriptor & PXN == 0
        };
        if write && execute {
            *violations += 1;
        }
    }
}

/// Remove a single 4 KiB page from the final higher-half map.
///
/// Used for task-stack guard pages. A 2 MiB block is split into 4 KiB pages
/// first, preserving the attributes of the rest of the block.
pub fn unmap_page(address: u64) -> Result<(), &'static str> {
    if !is_enabled() {
        return Err("MMU is not enabled");
    }
    if address % PAGE_SIZE != 0 {
        return Err("unmap address is not page aligned");
    }
    if !(KERNEL_VA_BASE..PHYSMAP_BASE).contains(&address) {
        return Err("unmap address is outside the kernel half");
    }
    let walker = Walker::new(PhysmapMemory);
    let mut allocator = PoolAllocator;
    walker
        .unmap_page(root(), address, &mut allocator)
        .map_err(|error| error.message())?;
    // SAFETY: the write above changed a live translation. Flush the whole
    // regime because kernel mappings are global (nG = 0).
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            options(nostack)
        );
    }
    Ok(())
}

/// Disable TTBR0 walks on this core. The kernel then has no low-address
/// identity map: every access must go through the higher half or the physmap.
pub fn seal_low_half() {
    if !is_enabled() {
        return;
    }
    let mut tcr: u64;
    // SAFETY: TCR_EL1 is writable at EL1.
    unsafe {
        asm!("mrs {}, TCR_EL1", out(reg) tcr, options(nomem, nostack));
        tcr |= 1 << 7; /* EPD0 */
        asm!("msr TCR_EL1, {}", in(reg) tcr, options(nomem, nostack));
        asm!("dsb ish", "isb", options(nomem, nostack));
        asm!("tlbi vmalle1", "dsb ish", "isb", options(nomem, nostack));
    }
}

/// Re-enable TTBR0 walks on this core. User entry needs the low half after the
/// kernel demo sealed it.
pub fn unseal_low_half() {
    if !is_enabled() {
        return;
    }
    let mut tcr: u64;
    // SAFETY: TCR_EL1 is writable at EL1.
    unsafe {
        asm!("mrs {}, TCR_EL1", out(reg) tcr, options(nomem, nostack));
        tcr &= !(1 << 7); /* EPD0 */
        asm!("msr TCR_EL1, {}", in(reg) tcr, options(nomem, nostack));
        asm!("dsb ish", "isb", options(nomem, nostack));
        asm!("tlbi vmalle1", "dsb ish", "isb", options(nomem, nostack));
    }
}

/// A user address space: one TTBR0 root and one ASID.
pub struct AddressSpace {
    pub root: u64,
    pub asid: u64,
    pub user: bool,
}

static ASID_NEXT: AtomicU64 = AtomicU64::new(1);

fn asid_limit() -> u64 {
    // SAFETY: written once during `init`.
    if unsafe { ASID16 } {
        0xffff
    } else {
        0xff
    }
}

fn allocate_asid() -> u64 {
    let limit = asid_limit();
    loop {
        let candidate = ASID_NEXT.fetch_add(1, Ordering::Relaxed) & limit;
        if candidate != 0 {
            return candidate;
        }
    }
}

impl AddressSpace {
    /// Allocate a fresh user root and ASID. The root starts empty.
    pub fn new_user() -> Result<Self, &'static str> {
        if !is_enabled() {
            return Err("MMU is not enabled");
        }
        let mut allocator = PoolAllocator;
        let root = allocator
            .alloc_table()
            .ok_or("out of page tables for a user root")?;
        Ok(Self {
            root,
            asid: allocate_asid(),
            user: true,
        })
    }

    /// Map `[virtual_address, +size)` into this address space.
    pub fn map(
        &mut self,
        virtual_address: u64,
        physical_address: u64,
        size: u64,
        perm: Perm,
    ) -> Result<(), &'static str> {
        if !self.user || !perm.user {
            return Err("user address spaces only accept user mappings");
        }
        if virtual_address >= PHYSMAP_BASE {
            return Err("user mapping is outside the low half");
        }
        let walker = Walker::new(PhysmapMemory);
        let mut allocator = PoolAllocator;
        walker
            .map_range(
                self.root,
                virtual_address,
                physical_address,
                size,
                perm,
                &mut allocator,
            )
            .map_err(|error| error.message())
    }

    /// Remove one page from this address space. Used by the 0.0.5 loader when
    /// it tears a process down.
    #[allow(dead_code)]
    pub fn unmap(&mut self, virtual_address: u64) -> Result<(), &'static str> {
        let walker = Walker::new(PhysmapMemory);
        let mut allocator = PoolAllocator;
        walker
            .unmap_page(self.root, virtual_address, &mut allocator)
            .map_err(|error| error.message())
    }

    #[allow(dead_code)] // 0.0.5 applies W^X to a loaded image.
    pub fn protect(&mut self, virtual_address: u64, perm: Perm) -> Result<(), &'static str> {
        let walker = Walker::new(PhysmapMemory);
        let mut allocator = PoolAllocator;
        walker
            .protect_page(self.root, virtual_address, perm, &mut allocator)
            .map_err(|error| error.message())
    }

    pub fn translate(&self, virtual_address: u64) -> Option<Translation> {
        Walker::new(PhysmapMemory).translate(self.root, virtual_address)
    }

    pub fn ttbr0_value(&self) -> u64 {
        // With TCR_EL1.A1 = 0 the ASID comes from TTBR0. ASID 0 is reserved
        // for the bootstrap identity map.
        if asid_limit() == 0xffff {
            self.root | (self.asid << 48)
        } else {
            self.root | (self.asid << 8)
        }
    }

    /// Install this address space in TTBR0 and return the previous value.
    pub fn activate(&mut self) -> u64 {
        let previous = read_ttbr0();
        let value = self.ttbr0_value();
        // SAFETY: `root` is a page-aligned table built from the pool.
        unsafe {
            asm!("msr TTBR0_EL1, {}", in(reg) value, options(nomem, nostack));
            asm!("dsb ish", "isb", options(nomem, nostack));
            asm!("tlbi vmalle1", "dsb ish", "isb", options(nomem, nostack));
        }
        previous
    }

    /// Restore a value returned by [`activate`].
    pub fn restore(&self, previous: u64) {
        // SAFETY: `previous` came from TTBR0_EL1.
        unsafe {
            asm!("msr TTBR0_EL1, {}", in(reg) previous, options(nomem, nostack));
            asm!("dsb ish", "isb", options(nomem, nostack));
            asm!("tlbi vmalle1", "dsb ish", "isb", options(nostack));
        }
    }

    /// Release the address space's identity. Tables stay in the fixed pool;
    /// 0.0.4 has no table reclamation.
    pub fn destroy(self) {
        let _ = self.root;
    }
}

fn read_ttbr0() -> u64 {
    let value: u64;
    // SAFETY: TTBR0_EL1 is readable at EL1.
    unsafe { asm!("mrs {}, TTBR0_EL1", out(reg) value, options(nomem, nostack)) };
    value
}

/// Install a precomputed TTBR0 value on the current core.
///
/// The scheduler stores each user entity's TTBR0 value so it can switch
/// address spaces from the timer path without taking the process-table lock.
/// A full local TLB invalidation is performed because the ASID changes with
/// the root in this milestone.
pub fn install_ttbr0(value: u64) -> u64 {
    let previous = read_ttbr0();
    if previous == value {
        return previous;
    }
    // SAFETY: `value` is a page-table root plus ASID produced by
    // `AddressSpace::ttbr0_value`.
    unsafe {
        asm!("msr TTBR0_EL1, {}", in(reg) value, options(nomem, nostack));
        asm!("dsb ish", "isb", options(nomem, nostack));
        asm!("tlbi vmalle1", "dsb ish", "isb", options(nomem, nostack));
    }
    previous
}

/// Current TTBR0 value, for diagnostics.
pub fn current_ttbr0() -> u64 {
    read_ttbr0()
}

/// Exercise the address-space machinery on the real MMU.
///
/// Allocates one physical page, maps it into a fresh user address space,
/// switches TTBR0, writes and reads through the user virtual address, restores
/// the bootstrap root and destroys the space.
pub fn address_space_self_test() -> Result<(), &'static str> {
    let physical = crate::phys::alloc_pages(1, PAGE_SIZE)
        .ok_or("address-space self-test: out of physical pages")?;
    let virtual_address = 0x0000_0000_0000_2000u64;

    let mut space = AddressSpace::new_user()?;
    let result = (|| -> Result<(), &'static str> {
        space.map(virtual_address, physical, PAGE_SIZE, Perm::USER_RW)?;
        let translation = space
            .translate(virtual_address)
            .ok_or("address-space self-test: mapping does not translate")?;
        if translation.physical != physical {
            return Err("address-space self-test: translation has the wrong physical address");
        }
        if !translation.perm.user || translation.perm.exec {
            return Err("address-space self-test: user permissions are wrong");
        }
        let previous = space.activate();
        // SAFETY: the page is mapped read/write at EL0 and EL1; TTBR0 now
        // describes this address space.
        let value = unsafe {
            core::ptr::write_volatile(virtual_address as *mut u64, 0x5a17_c0de_dead_beef);
            core::ptr::read_volatile(virtual_address as *const u64)
        };
        space.restore(previous);
        if value != 0x5a17_c0de_dead_beef {
            return Err("address-space self-test: readback mismatch");
        }
        Ok(())
    })();

    space.destroy();
    let _ = crate::phys::free_pages(physical, 1);
    result
}

/// Exercise the recoverable-fault probe path on the installed map.
///
/// Three deliberate faults must each reach the exception handler and resume in
/// the kernel: a read from an unmapped low address, a write to read-only
/// `.text`, and an instruction fetch from writable `.bss`. Each failure mode
/// must return `Err`; an `Ok` means the fault was not caught.
///
/// The probe state is global, so this runs on the boot core before any
/// secondary starts.
pub fn fault_self_test() -> Result<(), &'static str> {
    if super::probe_read(0x0000_0000_0000_1000).is_ok() {
        return Err("fault self-test: unmapped read succeeded");
    }

    let text = super::probe_read as fn(u64) -> Result<u64, ()> as usize as u64;
    if super::probe_write(text, 0).is_ok() {
        return Err("fault self-test: write to read-only kernel text succeeded");
    }

    let data = core::ptr::addr_of!(super::PROBE_ACTIVE) as u64;
    if super::probe_execute(data).is_ok() {
        return Err("fault self-test: executed writable data");
    }

    Ok(())
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

fn round_up(value: u64, align: u64) -> u64 {
    if value == 0 {
        return 0;
    }
    (value + align - 1) & !(align - 1)
}

const _: () = assert!(AF != 0);
const _: () = assert!(BLOCK_2M == 2 * 1024 * 1024);
