//! SketchOS UEFI loader.
//!
//! Responsibilities for milestone 0.0.4:
//!
//! 1. Locate and validate the ACPI RSDP and XSDT.
//! 2. Describe a debug UART (SPCR) and the UEFI GOP framebuffer.
//! 3. Load the separate ELF64 AArch64 kernel: a relocatable `ET_DYN` image is
//!    placed at a free physical base below 4 GiB and relocated, while legacy
//!    `ET_EXEC` images are loaded at their linked address.
//! 4. Obtain the final UEFI memory map and call `ExitBootServices` correctly.
//! 5. Build the versioned [`sketchos_bootinfo::BootInfo`] and enter the kernel.
//!
//! No UEFI Boot Service is used after `ExitBootServices`. Protocols, allocated
//! buffers and the filesystem are all dropped before the handoff.

#![no_std]
#![no_main]

extern crate alloc;

mod acpi;
mod elf;
mod handoff;
mod platform;

use alloc::string::ToString;
use sketchos_bootinfo::{
    uart_kind, BootInfo, BOOT_INFO_MAGIC, BOOT_INFO_SIZE, BOOT_INFO_VERSION, FLAG_FRAMEBUFFER,
    FLAG_IDENTITY_MAP, FLAG_QEMU_VIRT, FLAG_RSDP_VALID, FLAG_UART,
};
use uefi::boot::{self, AllocateType, MemoryType};
use uefi::fs::{FileSystem, PathBuf};
use uefi::prelude::*;
use uefi::CString16;

/// Kernel path on the boot volume.
const KERNEL_PATH: &str = "\\EFI\\SketchOS\\kernel.elf";

/// Marker file that asks the loader to occupy the old fixed link address.
/// `tools/smoke-test.ps1 -OccupyLegacy` creates it before booting.
const OCCUPY_LEGACY_PATH: &str = "\\EFI\\SketchOS\\occupy-legacy";

/// The fixed address used by the pre-relocatable 0.0.2 kernel. Occupying it
/// proves the loader no longer depends on firmware leaving this range free.
const LEGACY_KERNEL_ADDRESS: u64 = 0x4020_0000;

/// Fallback PL011 address for QEMU `virt` when firmware omits SPCR.
const QEMU_VIRT_PL011: u64 = 0x0900_0000;

#[entry]
fn main() -> Status {
    uefi::helpers::init().unwrap();

    // Do not let the firmware watchdog reset the machine while we are loading.
    let _ = boot::set_watchdog_timer(0, 0, None);

    uefi::println!("");
    uefi::println!("SketchOS UEFI loader 0.0.6 (AArch64)");
    uefi::println!("Firmware vendor: {}", system::firmware_vendor());
    uefi::println!("Firmware revision: 0x{:08x}", system::firmware_revision());

    let (loader_start, loader_end) = platform::loader_range().unwrap_or((0, 0));
    if loader_start != 0 {
        uefi::println!(
            "Loader image: 0x{:016x}..0x{:016x} ({} KiB)",
            loader_start,
            loader_end,
            (loader_end - loader_start) / 1024
        );
    }

    let acpi = acpi::discover();
    let qemu = acpi.as_ref().map(|a| a.qemu).unwrap_or(false);

    let mut info = BootInfo::zeroed();
    info.magic = BOOT_INFO_MAGIC;
    info.version = BOOT_INFO_VERSION;
    info.size = BOOT_INFO_SIZE;
    info.flags = FLAG_IDENTITY_MAP;
    info.loader_phys_start = loader_start;
    info.loader_phys_end = loader_end;
    info.firmware_revision = system::firmware_revision();
    info.firmware_vendor = platform::firmware_vendor_ascii();

    if let Some(acpi) = &acpi {
        info.rsdp_address = acpi.rsdp_address;
        info.acpi_oem_id = acpi.oem_id;
        info.acpi_revision = acpi.revision;
        if acpi.checksum_ok {
            info.flags |= FLAG_RSDP_VALID;
        }
        if acpi.qemu {
            info.flags |= FLAG_QEMU_VIRT;
        }

        if let Some(spcr) = &acpi.spcr {
            info.uart_base = spcr.base;
            info.uart_kind = spcr.kind;
            if spcr.base != 0 {
                info.flags |= FLAG_UART;
            }
        }
    }

    // QEMU's ACPI normally carries SPCR. If it does not, the OEM ID still
    // identifies the platform unambiguously, and PL011 at 0x09000000 is part
    // of the QEMU virt contract.
    if info.uart_kind == uart_kind::NONE && qemu {
        info.uart_base = QEMU_VIRT_PL011;
        info.uart_kind = uart_kind::PL011;
        info.flags |= FLAG_UART;
    }

    if let Some(fb) = platform::framebuffer_info() {
        uefi::println!(
            "GOP framebuffer: {}x{} stride {} {} @ 0x{:016x} ({} MiB)",
            fb.width,
            fb.height,
            fb.stride,
            sketchos_bootinfo::pixel_format::name(fb.pixel_format),
            fb.address,
            fb.size / (1024 * 1024)
        );
        info.framebuffer = fb;
        info.flags |= FLAG_FRAMEBUFFER;
    } else {
        uefi::println!("GOP framebuffer: not available");
    }

    occupy_legacy_address_if_requested();

    let kernel = match load_kernel(KERNEL_PATH) {
        Ok(k) => k,
        Err(err) => {
            uefi::println!("ERROR: failed to load kernel: {}", err);
            return Status::LOAD_ERROR;
        }
    };

    info.kernel_phys_start = kernel.start;
    info.kernel_phys_end = kernel.end;

    uefi::println!(
        "Kernel: {} bytes, {} segments, {} KiB physical at 0x{:016x}..0x{:016x}",
        kernel.file_size,
        kernel.segment_count,
        (kernel.end - kernel.start) / 1024,
        kernel.start,
        kernel.end
    );
    uefi::println!(
        "Kernel image: {}, {} relocations",
        if kernel.relocatable { "ET_DYN" } else { "ET_EXEC" },
        kernel.relocation_count
    );
    uefi::println!(
        "Kernel entry: physical 0x{:016x}, virtual 0x{:016x}",
        kernel.entry,
        kernel.entry_virt
    );
    uefi::println!(
        "Kernel virtual range: 0x{:016x}..0x{:016x}",
        kernel.virt_start,
        kernel.virt_end
    );

    if info.flags & FLAG_UART != 0 {
        uefi::println!(
            "Debug UART: {} @ 0x{:016x}",
            uart_kind::name(info.uart_kind),
            info.uart_base
        );
    } else {
        uefi::println!("Debug UART: none described by firmware");
    }

    // Put the final handoff marker at the absolute top-left of the UEFI
    // text console. Keep it visible while ExitBootServices and the branch to
    // the kernel happen; if it remains here, the kernel entry did not report
    // progress back to the display.
    system::with_stdout(|stdout| {
        let _ = stdout.set_cursor_position(0, 0);
    });
    uefi::println!("HANDOFF: ENTERING KERNEL");

    // `enter` consumes every remaining Boot Service resource and never
    // returns. Nothing below may allocate or print through UEFI.
    unsafe { handoff::enter(info, kernel) }
}

/// Optionally reserve the old fixed kernel address before the kernel is loaded.
///
/// This is a boot-time self-test, not production behavior: the marker file is
/// created only by `tools/smoke-test.ps1 -OccupyLegacy`. If the allocation
/// succeeds, a non-relocatable loader would fail and a relocatable one boots
/// somewhere else.
fn occupy_legacy_address_if_requested() {
    let fs = match boot::get_image_file_system(boot::image_handle()) {
        Ok(fs) => fs,
        Err(_) => return,
    };
    let mut fs = FileSystem::new(fs);
    let path = match CString16::try_from(OCCUPY_LEGACY_PATH) {
        Ok(path) => PathBuf::from(path),
        Err(_) => return,
    };
    if fs.read(&path).is_err() {
        return;
    }

    match boot::allocate_pages(
        AllocateType::Address(LEGACY_KERNEL_ADDRESS),
        MemoryType::LOADER_DATA,
        1,
    ) {
        Ok(pointer) => uefi::println!(
            "Relocation test: occupied legacy kernel address 0x{:016x} at {:p}",
            LEGACY_KERNEL_ADDRESS,
            pointer
        ),
        Err(error) => uefi::println!(
            "Relocation test: could not occupy 0x{:016x}: {}",
            LEGACY_KERNEL_ADDRESS,
            error
        ),
    }
}

/// Read and load the kernel ELF image. The file buffer and filesystem protocol
/// are dropped here, before `ExitBootServices`.
fn load_kernel(path: &str) -> Result<elf::LoadedKernel, alloc::string::String> {
    let fs = boot::get_image_file_system(boot::image_handle())
        .map_err(|e| alloc::format!("cannot open boot volume: {e}"))?;
    let mut fs = FileSystem::new(fs);

    let path = PathBuf::from(
        CString16::try_from(path).map_err(|e| alloc::format!("invalid kernel path: {e}"))?,
    );
    let data = fs
        .read(&path)
        .map_err(|e| alloc::format!("cannot read {}: {e}", path))?;

    uefi::println!("Kernel file: {} ({} bytes)", path, data.len());
    let loaded = elf::load(&data);
    drop(data);
    drop(fs);
    loaded.map_err(|e| e.message().to_string())
}
