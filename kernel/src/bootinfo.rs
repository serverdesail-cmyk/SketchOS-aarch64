//! Boot-information validation.
//!
//! The loader is trusted, but the kernel still validates every structural
//! field before dereferencing anything. A bad pointer must be rejected with a
//! diagnostic, not silently followed.
//!
//! Since 0.0.4 the kernel runs in the higher half and receives *physical*
//! addresses from the loader. Every access in this module goes through the
//! physical map (`PHYSMAP_BASE + physical`), never through a direct pointer.

use core::mem::{align_of, size_of};

use sketchos_bootinfo::{
    phys_to_virt, uart_kind, BootInfo, MemoryDescriptor, BOOT_INFO_MAGIC, BOOT_INFO_VERSION,
    FLAG_FRAMEBUFFER, FLAG_HIGHER_HALF, FLAG_MEMORY_MAP, FLAG_RSDP_VALID, FLAG_UART,
    KERNEL_VA_BASE, MAX_PHYS, PAGE_SIZE, PHYSMAP_BASE,
};

const MAX_BOOT_INFO_SIZE: u32 = 4096;
const MAX_MEMORY_DESCRIPTORS: u64 = 1 << 20;

/// Validate and copy the boot record passed by the loader.
pub fn validate(address: u64) -> Result<BootInfo, &'static str> {
    if address == 0 {
        return Err("boot-info pointer is null");
    }
    if address % align_of::<BootInfo>() as u64 != 0 {
        return Err("boot-info pointer is misaligned");
    }

    // SAFETY: the pointer is non-null, aligned, and lies in the physical map
    // the loader installed before entering the kernel.
    let mut info = unsafe { core::ptr::read_volatile(phys_to_virt(address) as *const BootInfo) };

    if info.magic != BOOT_INFO_MAGIC {
        return Err("boot-info magic mismatch");
    }
    if info.version != BOOT_INFO_VERSION {
        return Err("unsupported boot-info version");
    }
    if info.size < size_of::<BootInfo>() as u32 || info.size > MAX_BOOT_INFO_SIZE {
        return Err("boot-info size is invalid");
    }
    if !info.has_flag(FLAG_MEMORY_MAP) {
        return Err("boot info contains no memory map");
    }
    if !info.has_flag(FLAG_HIGHER_HALF) {
        return Err("boot info does not describe a higher-half map");
    }

    if info.ttbr0_boot_root == 0 || info.ttbr0_boot_root % PAGE_SIZE != 0 {
        return Err("bootstrap TTBR0 root is invalid");
    }
    if info.ttbr1_root == 0 || info.ttbr1_root % PAGE_SIZE != 0 {
        return Err("kernel TTBR1 root is invalid");
    }
    if info.physmap_base != PHYSMAP_BASE {
        return Err("physical-map base is not the kernel base");
    }
    if info.physmap_size == 0
        || info.physmap_size % PAGE_SIZE != 0
        || info.physmap_size > MAX_PHYS
    {
        return Err("physical-map size is invalid");
    }
    if info.physmap_base.checked_add(info.physmap_size).is_none() {
        return Err("physical-map range overflows");
    }

    if info.kernel_phys_start == 0 || info.kernel_phys_start >= info.kernel_phys_end {
        return Err("kernel physical range is invalid");
    }
    if info.kernel_phys_end > MAX_PHYS || info.kernel_phys_end > info.physmap_size {
        return Err("kernel is outside the physical map");
    }
    let expected_va_start = KERNEL_VA_BASE
        .checked_add(info.kernel_phys_start)
        .ok_or("kernel virtual range overflows")?;
    let expected_va_end = KERNEL_VA_BASE
        .checked_add(info.kernel_phys_end)
        .ok_or("kernel virtual range overflows")?;
    if info.kernel_va_start != expected_va_start || info.kernel_va_end != expected_va_end {
        return Err("kernel virtual range does not match the physical range");
    }

    // TCR must keep both TTBR walks enabled and use the 4 KiB granule for both
    // regimes. The kernel reuses the loader's value rather than inventing a
    // second one.
    if info.tcr & (1 << 7) != 0 || info.tcr & (1 << 23) != 0 {
        return Err("TCR disables a translation table walk");
    }
    if (info.tcr >> 14) & 0x3 != 0 || (info.tcr >> 30) & 0x3 != 2 {
        return Err("TCR does not use the 4 KiB granule");
    }
    if info.mair == 0 {
        return Err("MAIR is zero");
    }

    let map = info.memory_map;
    if map.address == 0 {
        return Err("memory-map pointer is null");
    }
    if map.address % 8 != 0 {
        return Err("memory-map pointer is misaligned");
    }
    if map.entry_size as usize != size_of::<MemoryDescriptor>() {
        return Err("memory-map entry size mismatch");
    }
    if map.count == 0 || map.count > MAX_MEMORY_DESCRIPTORS {
        return Err("memory-map entry count is invalid");
    }
    let map_bytes = map
        .count
        .checked_mul(map.entry_size as u64)
        .ok_or("memory-map byte size overflow")?;
    let map_end = map
        .address
        .checked_add(map_bytes)
        .ok_or("memory-map address overflow")?;
    if map_end > info.physmap_size || map_end > MAX_PHYS {
        return Err("memory map is outside the physical map");
    }

    // Optional descriptions are downgraded to warnings rather than refusing
    // to boot; they are not needed for the memory map or kernel handoff.
    if info.has_flag(FLAG_UART) {
        let usable = info.uart_base != 0
            && info.uart_kind != uart_kind::NONE
            && (info.uart_kind != uart_kind::PL011 || info.uart_base % 4 == 0);
        if !usable {
            info.flags &= !FLAG_UART;
        }
    }

    if info.has_flag(FLAG_FRAMEBUFFER) {
        let fb = info.framebuffer;
        let bytes = (fb.stride as u64)
            .checked_mul(fb.height as u64)
            .and_then(|pixels| pixels.checked_mul(4));
        let usable = fb.address != 0
            && fb.width != 0
            && fb.height != 0
            && fb.stride >= fb.width
            && bytes.map_or(false, |needed| fb.size >= needed)
            && fb
                .address
                .checked_add(fb.size)
                .map_or(false, |end| end <= info.physmap_size);
        if !usable {
            info.flags &= !FLAG_FRAMEBUFFER;
        }
    }

    // The loader validated the RSDP checksums before setting the flag. The
    // kernel only needs a non-null address; it must not reject firmware that
    // places the RSDP on an 8-byte rather than 16-byte boundary. QEMU/EDK2
    // does exactly that (`0x...018`), and the reader performs unaligned-safe
    // accesses.
    if info.has_flag(FLAG_RSDP_VALID) && info.rsdp_address == 0 {
        info.flags &= !FLAG_RSDP_VALID;
    }

    Ok(info)
}
