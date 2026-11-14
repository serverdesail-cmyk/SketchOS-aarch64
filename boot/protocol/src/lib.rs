//! SketchOS boot-information ABI.
//!
//! This crate is shared by the UEFI loader and the freestanding kernel. It is
//! deliberately dependency-free and `no_std` so that both sides agree on a
//! single, versioned C layout.
//!
//! # Lifetime and ownership
//!
//! The loader allocates the [`BootInfo`] structure and the memory-map array
//! with UEFI `AllocatePages(..., EfiLoaderData, ...)` and never frees them.
//! Both allocations are therefore valid after `ExitBootServices` and are
//! described by the final UEFI memory map as loader data.
//!
//! The kernel must treat every pointer in this structure as a physical
//! address. Since milestone 0.0.4 the loader also describes the virtual
//! address space it built: a higher-half kernel map plus a linear physical
//! map. Physical addresses reach the kernel only through the physical map
//! (`PHYSMAP_BASE + physical`), never by direct dereference.

#![no_std]

use core::mem::size_of;

/// `"SKBOOT01"` interpreted as a little-endian `u64`.
pub const BOOT_INFO_MAGIC: u64 = u64::from_le_bytes(*b"SKBOOT01");

/// Current boot-information layout version.
pub const BOOT_INFO_VERSION: u32 = 2;

/// Minimum byte size a consumer must accept for version 2.
pub const BOOT_INFO_SIZE: u32 = size_of::<BootInfo>() as u32;

/// Page granule used by the kernel and loader page tables.
pub const PAGE_SIZE: u64 = 4096;

/// Base of the higher-half kernel image mapping.
///
/// The kernel is linked at virtual address zero and the loader places it at a
/// physical base. Its runtime virtual address is `KERNEL_VA_BASE + physical`.
pub const KERNEL_VA_BASE: u64 = 0xFFFF_8000_0000_0000;

/// Base of the linear physical map. Every physical byte `p` is reachable at
/// `PHYSMAP_BASE + p`; the kernel requires this for MMIO, firmware tables and
/// page-table walks.
pub const PHYSMAP_BASE: u64 = 0xFFFF_C000_0000_0000;

/// Highest physical address the kernel page tables can describe. The overall
/// VA space is 48-bit, split evenly between TTBR0 and TTBR1. 512 GiB matches
/// the span the 0.0.3 identity map already proved on QEMU and the laptop.
pub const MAX_PHYS: u64 = 512 * 1024 * 1024 * 1024;

/// Translate a physical address to its kernel virtual address through the
/// physical map.
pub const fn phys_to_virt(physical: u64) -> u64 {
    PHYSMAP_BASE.wrapping_add(physical)
}

/// Translate a kernel virtual address in the physical map back to physical.
/// Returns `None` when the address is not inside the physical map.
pub const fn virt_to_phys(virtual_address: u64) -> Option<u64> {
    if virtual_address < PHYSMAP_BASE {
        return None;
    }
    let physical = virtual_address - PHYSMAP_BASE;
    if physical < MAX_PHYS {
        Some(physical)
    } else {
        None
    }
}

/// Translate a kernel image virtual address to its physical address.
pub const fn kernel_virt_to_phys(virtual_address: u64) -> Option<u64> {
    if virtual_address < KERNEL_VA_BASE {
        return None;
    }
    let physical = virtual_address - KERNEL_VA_BASE;
    if physical < MAX_PHYS {
        Some(physical)
    } else {
        None
    }
}

/// `memory_map` points at a valid array of [`MemoryDescriptor`].
pub const FLAG_MEMORY_MAP: u64 = 1 << 0;
/// `rsdp_address` points at a checksum-valid ACPI RSDP.
pub const FLAG_RSDP_VALID: u64 = 1 << 1;
/// `framebuffer` contains a usable UEFI GOP framebuffer description.
pub const FLAG_FRAMEBUFFER: u64 = 1 << 2;
/// `uart_base`/`uart_kind` describe a debug UART.
pub const FLAG_UART: u64 = 1 << 3;
/// Firmware handed off with an identity-mapped MMU and caches enabled.
pub const FLAG_IDENTITY_MAP: u64 = 1 << 4;
/// The platform was identified as QEMU `virt` (via the ACPI OEM ID).
pub const FLAG_QEMU_VIRT: u64 = 1 << 5;
/// The loader built a higher-half kernel map and a linear physical map. When
/// set, the trailing translation fields are valid and the kernel must not
/// assume that virtual addresses equal physical addresses.
pub const FLAG_HIGHER_HALF: u64 = 1 << 6;

/// Normalized memory kinds. Values intentionally mirror UEFI `EFI_MEMORY_TYPE`
/// for the standard range so the loader conversion is mechanical.
pub mod memory_kind {
    pub const RESERVED: u32 = 0;
    pub const LOADER_CODE: u32 = 1;
    pub const LOADER_DATA: u32 = 2;
    pub const BOOT_SERVICES_CODE: u32 = 3;
    pub const BOOT_SERVICES_DATA: u32 = 4;
    pub const RUNTIME_SERVICES_CODE: u32 = 5;
    pub const RUNTIME_SERVICES_DATA: u32 = 6;
    pub const CONVENTIONAL: u32 = 7;
    pub const UNUSABLE: u32 = 8;
    pub const ACPI_RECLAIM: u32 = 9;
    pub const ACPI_NVS: u32 = 10;
    pub const MMIO: u32 = 11;
    pub const MMIO_PORT_SPACE: u32 = 12;
    pub const PAL_CODE: u32 = 13;
    pub const PERSISTENT: u32 = 14;
    /// UEFI custom/OEM type or any value this kernel does not understand.
    pub const UNKNOWN: u32 = 255;

    pub fn name(kind: u32) -> &'static str {
        match kind {
            RESERVED => "reserved",
            LOADER_CODE => "loader-code",
            LOADER_DATA => "loader-data",
            BOOT_SERVICES_CODE => "boot-services-code",
            BOOT_SERVICES_DATA => "boot-services-data",
            RUNTIME_SERVICES_CODE => "runtime-code",
            RUNTIME_SERVICES_DATA => "runtime-data",
            CONVENTIONAL => "conventional",
            UNUSABLE => "unusable",
            ACPI_RECLAIM => "acpi-reclaim",
            ACPI_NVS => "acpi-nvs",
            MMIO => "mmio",
            MMIO_PORT_SPACE => "mmio-port",
            PAL_CODE => "pal-code",
            PERSISTENT => "persistent",
            _ => "unknown",
        }
    }
}

/// Debug UART kinds. Only `PL011` is driven by the 0.0.2 kernel.
pub mod uart_kind {
    pub const NONE: u32 = 0;
    /// ARM PL011 (ACPI SPCR interface type 0x03).
    pub const PL011: u32 = 1;
    /// 16550-compatible family (SPCR interface types 0x00..=0x02).
    pub const UART16550: u32 = 2;
    /// ARM SBSA generic UART (SPCR interface type 0x0e).
    pub const SBSA: u32 = 3;
    /// Any other firmware-described UART, recorded but not driven yet.
    pub const OTHER: u32 = 4;
    /// Qualcomm GENI Serial Engine UART (ACPI SPCR interface type 0x13).
    /// The SPCR type identifies the SDM845 7.372 MHz UART programming model.
    pub const SDM845_GENI: u32 = 5;

    pub fn name(kind: u32) -> &'static str {
        match kind {
            NONE => "none",
            PL011 => "pl011",
            UART16550 => "16550",
            SBSA => "sbsa",
            OTHER => "other",
            SDM845_GENI => "sdm845-geni",
            _ => "unknown",
        }
    }
}

/// UEFI GOP pixel formats, normalized.
pub mod pixel_format {
    pub const UNKNOWN: u32 = 0;
    pub const RGB: u32 = 1;
    pub const BGR: u32 = 2;
    pub const BITMASK: u32 = 3;

    pub fn name(format: u32) -> &'static str {
        match format {
            RGB => "rgb",
            BGR => "bgr",
            BITMASK => "bitmask",
            _ => "unknown",
        }
    }
}

/// A physical array of [`MemoryDescriptor`] entries.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MemoryRegion {
    /// Physical address of the first descriptor.
    pub address: u64,
    /// Number of descriptors.
    pub count: u64,
    /// Size of one descriptor in bytes, for forward compatibility.
    pub entry_size: u32,
    pub _reserved: u32,
}

impl MemoryRegion {
    pub const EMPTY: Self = Self {
        address: 0,
        count: 0,
        entry_size: 0,
        _reserved: 0,
    };
}

/// One normalized firmware memory-map entry.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MemoryDescriptor {
    /// One of the [`memory_kind`] constants.
    pub kind: u32,
    pub _reserved: u32,
    /// First physical byte of the range (4 KiB aligned).
    pub physical_start: u64,
    /// Length in 4 KiB pages.
    pub page_count: u64,
    /// UEFI capability attributes, passed through for the memory manager.
    pub attributes: u64,
}

impl MemoryDescriptor {
    pub const ZERO: Self = Self {
        kind: 0,
        _reserved: 0,
        physical_start: 0,
        page_count: 0,
        attributes: 0,
    };

    /// End physical address, or `None` on overflow.
    pub const fn end(&self) -> Option<u64> {
        self.physical_start
            .checked_add(self.page_count.saturating_mul(4096))
    }

    pub const fn size_bytes(&self) -> u64 {
        self.page_count.saturating_mul(4096)
    }
}

/// Framebuffer description inherited from the UEFI GOP.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FramebufferInfo {
    /// Physical base address of the linear framebuffer.
    pub address: u64,
    pub width: u32,
    pub height: u32,
    /// Pixels per scan line (may exceed `width`).
    pub stride: u32,
    /// One of the [`pixel_format`] constants.
    pub pixel_format: u32,
    /// Mapped framebuffer size in bytes.
    pub size: u64,
}

impl FramebufferInfo {
    pub const EMPTY: Self = Self {
        address: 0,
        width: 0,
        height: 0,
        stride: 0,
        pixel_format: pixel_format::UNKNOWN,
        size: 0,
    };
}

/// Versioned handoff record passed to the kernel in `x0`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct BootInfo {
    /// [`BOOT_INFO_MAGIC`].
    pub magic: u64,
    /// [`BOOT_INFO_VERSION`].
    pub version: u32,
    /// Size of this structure in bytes.
    pub size: u32,
    /// [`FLAG_*`](FLAG_MEMORY_MAP) bits.
    pub flags: u64,
    pub memory_map: MemoryRegion,
    /// Physical address of the ACPI RSDP, or zero.
    pub rsdp_address: u64,
    /// Kernel image physical range, inclusive start / exclusive end.
    pub kernel_phys_start: u64,
    pub kernel_phys_end: u64,
    /// Loader image physical range, if known (zero otherwise).
    pub loader_phys_start: u64,
    pub loader_phys_end: u64,
    /// Debug UART physical base, or zero.
    pub uart_base: u64,
    /// One of the [`uart_kind`] constants.
    pub uart_kind: u32,
    /// UEFI firmware revision (`EFI_SYSTEM_TABLE.Revision`).
    pub firmware_revision: u32,
    pub framebuffer: FramebufferInfo,
    /// Firmware vendor as ASCII, NUL padded.
    pub firmware_vendor: [u8; 64],
    /// ACPI RSDP OEM ID (6 ASCII bytes).
    pub acpi_oem_id: [u8; 6],
    /// ACPI RSDP revision.
    pub acpi_revision: u8,
    pub _reserved: u8,
    /// Physical address of the bootstrap TTBR0 root (boot identity map).
    pub ttbr0_boot_root: u64,
    /// Physical address of the TTBR1 root (kernel image + physical map).
    pub ttbr1_root: u64,
    /// `TCR_EL1` value the loader's tables were built for.
    pub tcr: u64,
    /// `MAIR_EL1` value matching the attribute indices in the tables.
    pub mair: u64,
    /// Inclusive virtual start of the kernel image mapping.
    pub kernel_va_start: u64,
    /// Exclusive virtual end of the kernel image mapping.
    pub kernel_va_end: u64,
    /// Base virtual address of the linear physical map.
    pub physmap_base: u64,
    /// Number of physical bytes covered by the linear map.
    pub physmap_size: u64,
}

impl BootInfo {
    /// A fully zeroed record. The loader fills every meaningful field.
    pub const fn zeroed() -> Self {
        Self {
            magic: 0,
            version: 0,
            size: 0,
            flags: 0,
            memory_map: MemoryRegion::EMPTY,
            rsdp_address: 0,
            kernel_phys_start: 0,
            kernel_phys_end: 0,
            loader_phys_start: 0,
            loader_phys_end: 0,
            uart_base: 0,
            uart_kind: uart_kind::NONE,
            firmware_revision: 0,
            framebuffer: FramebufferInfo::EMPTY,
            firmware_vendor: [0; 64],
            acpi_oem_id: [0; 6],
            acpi_revision: 0,
            _reserved: 0,
            ttbr0_boot_root: 0,
            ttbr1_root: 0,
            tcr: 0,
            mair: 0,
            kernel_va_start: 0,
            kernel_va_end: 0,
            physmap_base: 0,
            physmap_size: 0,
        }
    }

    /// Check the fixed header. Pointer contents still require validation.
    pub fn header_valid(&self) -> bool {
        self.magic == BOOT_INFO_MAGIC
            && self.version == BOOT_INFO_VERSION
            && self.size as usize >= size_of::<Self>()
    }

    pub const fn has_flag(&self, flag: u64) -> bool {
        self.flags & flag != 0
    }
}

const _: () = assert!(size_of::<MemoryRegion>() == 24);
const _: () = assert!(size_of::<MemoryDescriptor>() == 32);
const _: () = assert!(size_of::<FramebufferInfo>() == 32);
const _: () = assert!(size_of::<BootInfo>() == 272);
