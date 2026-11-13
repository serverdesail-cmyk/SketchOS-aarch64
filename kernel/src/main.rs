//! SketchOS freestanding AArch64 kernel, milestone 0.0.4.
//!
//! Entered by the UEFI loader with the physical address of a versioned
//! `sketchos_bootinfo::BootInfo` record in `x0` and the exception level
//! firmware used in `x1`, under the loader's bootstrap higher-half map and with
//! Boot Services already gone. `boot.S` drops EL2 to EL1 before calling
//! `kmain`.
//!
//! This milestone validates the version-2 handoff, re-checks ACPI, decodes the
//! platform inventory, rebuilds the final W^X page tables and the 512 GiB
//! physical map, installs recoverable fault routing, exercises the ASID-based
//! address-space machinery, then runs the SMP/timer/scheduler demo on
//! top of the new map. Drivers and userspace remain later milestones.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

mod acpi;
mod arch;
mod bootinfo;
mod console;
mod cpu;
mod drm;
mod drivers;
mod ext2;
mod fat32;
mod fb;
mod font8x8;
mod heap;
mod memory;
mod memmap;
mod net;
mod nvme;
mod panic;
mod partition;
mod pci;
mod percpu;
mod phys;
mod process;
mod sched;
mod security;
#[cfg(feature = "shell")]
mod shell;
mod selftest;
mod smp;
mod storage;
mod sync;
mod syscall;
mod topology;

use sketchos_bootinfo::{
    uart_kind, BootInfo, FLAG_FRAMEBUFFER, FLAG_HIGHER_HALF, FLAG_QEMU_VIRT, FLAG_RSDP_VALID,
    FLAG_UART, PHYSMAP_BASE,
};

#[no_mangle]
pub extern "C" fn kmain(boot_info_addr: u64, entry_el: u64) -> ! {
    let info = match bootinfo::validate(boot_info_addr) {
        Ok(info) => {
            info
        }
        // Nothing is known about the UART yet, so there is nowhere safe to
        // report the reason. The loader's own output remains on screen.
        Err(_) => {
            arch::halt()
        }
    };

    // Install the fault table before touching any device. A bad UART or
    // framebuffer address then produces a diagnostic instead of a silent hang.
    let vectors = arch::aarch64::install_exception_vectors();

    if info.has_flag(FLAG_UART) && info.uart_kind == uart_kind::PL011 {
        console::init(info.uart_base);
    }

    let counter_frequency = arch::aarch64::counter_frequency();
    let start_ticks = arch::aarch64::counter();

    // Decode ACPI while the firmware identity map is still active. This reads
    // only firmware tables in RAM and never touches a device.
    let platform = acpi::discover(&info);

    // Replace the firmware map with kernel-owned tables. A failure is not
    // fatal: the kernel continues under the firmware map and reports why.
    let mmu_result = arch::aarch64::mmu::init(&info, platform.as_ref());

    // The framebuffer is initialized after the MMU so the console draws through
    // the final Normal Non-cacheable mapping rather than the firmware's.
    let mut framebuffer_console = false;
    if info.has_flag(FLAG_FRAMEBUFFER) {
        match console::init_framebuffer(&info.framebuffer) {
            Ok(()) => framebuffer_console = console::framebuffer_active(),
            Err(reason) => {
                // The UART may not exist on this machine, so this line can be
                // invisible on real hardware. It is still useful under QEMU.
                println!("Framebuffer console unavailable: {}", reason);
            }
        }
    }

    println!();
    println!("SketchOS kernel 0.0.7 (AArch64)");
    println!(
        "Exceptions: VBAR_EL{} 0x{:016x}, table aligned: {}",
        arch::aarch64::current_el(),
        vectors,
        yes_no(vectors % 2048 == 0)
    );
    match mmu_result {
        Ok(()) => arch::aarch64::mmu::report(),
        Err(reason) => println!("MMU: unavailable: {}", reason),
    }
    if info.has_flag(FLAG_FRAMEBUFFER) {
        println!(
            "Framebuffer console: {}",
            if framebuffer_console {
                "active"
            } else {
                "unavailable"
            }
        );
    }
    println!(
        "Boot info: 0x{:016x}, version {}, {} bytes, flags 0x{:08x}",
        boot_info_addr, info.version, info.size, info.flags
    );
    println!(
        "Firmware: \"{}\", revision 0x{:08x}",
        ascii(&info.firmware_vendor),
        info.firmware_revision
    );
    println!(
        "CPU: EL{}, MPIDR 0x{:016x}, counter {} Hz{}",
        arch::aarch64::current_el(),
        arch::aarch64::mpidr(),
        counter_frequency,
        if info.has_flag(FLAG_QEMU_VIRT) {
            ", QEMU virt"
        } else {
            ""
        }
    );
    println!(
        "Entry exception level: EL{}, running at EL{}",
        entry_el,
        arch::aarch64::current_el()
    );

    let running_at = kmain as *const () as usize as u64;
    let inside_kernel =
        running_at >= info.kernel_va_start && running_at < info.kernel_va_end;
    println!(
        "Kernel: physical 0x{:016x}-0x{:016x} ({} KiB), virtual 0x{:016x}-0x{:016x}",
        info.kernel_phys_start,
        info.kernel_phys_end,
        (info.kernel_phys_end - info.kernel_phys_start) / 1024,
        info.kernel_va_start,
        info.kernel_va_end
    );
    println!(
        "  running at 0x{:016x}, inside the higher-half image: {}",
        running_at,
        yes_no(inside_kernel)
    );
    if info.has_flag(FLAG_HIGHER_HALF) {
        println!(
            "  higher half: physmap 0x{:016x}-0x{:016x} ({} GiB), boot root 0x{:016x}, kernel root 0x{:016x}",
            PHYSMAP_BASE,
            PHYSMAP_BASE + info.physmap_size,
            info.physmap_size / (1024 * 1024 * 1024),
            info.ttbr0_boot_root,
            info.ttbr1_root
        );
        println!(
            "  TCR 0x{:016x}, MAIR 0x{:016x}",
            info.tcr, info.mair
        );
    } else {
        println!("  higher-half translation was not described by the loader");
    }

    if info.loader_phys_start != 0 {
        println!(
            "Loader: 0x{:016x}-0x{:016x} ({} KiB)",
            info.loader_phys_start,
            info.loader_phys_end,
            (info.loader_phys_end - info.loader_phys_start) / 1024
        );
    }

    if info.has_flag(FLAG_UART) {
        println!(
            "Debug UART: {} @ 0x{:016x}{}",
            uart_kind::name(info.uart_kind),
            info.uart_base,
            if info.uart_kind == uart_kind::PL011 {
                ""
            } else {
                " (driver not implemented yet)"
            }
        );
    } else {
        println!("Debug UART: none described by firmware");
    }

    if info.has_flag(FLAG_FRAMEBUFFER) {
        println!(
            "Framebuffer: {}x{}, stride {}, {} @ 0x{:016x}, {} MiB",
            info.framebuffer.width,
            info.framebuffer.height,
            info.framebuffer.stride,
            sketchos_bootinfo::pixel_format::name(info.framebuffer.pixel_format),
            info.framebuffer.address,
            info.framebuffer.size / (1024 * 1024)
        );
    } else {
        println!("Framebuffer: none described by the loader");
    }

    if info.has_flag(FLAG_RSDP_VALID) {
        println!(
            "ACPI: revision {}, OEM \"{}\"",
            info.acpi_revision,
            ascii(&info.acpi_oem_id)
        );
    }

    if let Some(platform) = platform.as_ref() {
        let mpidr = arch::aarch64::mpidr();
        match platform.current_cpu(mpidr) {
            Some(current) => println!(
                "Running on ACPI CPU interface {}, UID {}, efficiency class {}",
                current.interface, current.uid, current.efficiency
            ),
            None => println!(
                "Running CPU MPIDR 0x{:016x} has no MADT entry",
                mpidr
            ),
        }
        report_firmware_devices(platform);
        cpu::report(platform);
        memory::report(&info, platform);
        pci::report(platform);
        drivers::report(platform);
        net::report();
        security::report();
    }
    memmap::report(&info, boot_info_addr);

    let elapsed = arch::aarch64::counter().wrapping_sub(start_ticks);
    let micros = if counter_frequency == 0 {
        0
    } else {
        (elapsed as u128 * 1_000_000 / counter_frequency as u128) as u64
    };
    println!();
    println!(
        "Kernel inventory time: {} us ({} counter ticks at {} Hz)",
        micros, elapsed, counter_frequency
    );

    println!("BOOT STAGE: inventory complete; installing ACPI platform");

    // The interrupt controller, timer, memory allocators and SMP startup
    // require an ACPI description and EL1. A failure here is fatal to the
    // milestone because the kernel has no other way to regain control.
    let platform = match platform.as_ref() {
        Some(platform) => acpi::install(platform),
        None => fatal(&info, None, "no ACPI platform description"),
    };

    println!("BOOT STAGE: ACPI platform installed");
    smp::set_qemu(info.has_flag(FLAG_QEMU_VIRT));

    // Physical memory and the kernel heap are brought up before any secondary
    // is started. The boot self-tests release every allocation they make.
    println!("BOOT STAGE: initializing physical memory allocator");
    if let Err(_reason) = phys::init(&info, boot_info_addr) {
        fatal(
            &info,
            Some(platform),
            "physical allocator initialization failed",
        );
    }
    println!("BOOT STAGE: physical allocator ready; running memory self-test");
    if let Err(reason) = selftest::physical(&info, boot_info_addr) {
        fatal(&info, Some(platform), reason);
    }

    // Exercise a real user address space (fresh TTBR0 root plus ASID) while the
    // bootstrap identity root is still installed. The self-test restores the
    // root before returning, so sealing TTBR0 later is safe.
    println!("BOOT STAGE: memory self-test passed; checking address spaces");
    match arch::aarch64::mmu::address_space_self_test() {
        Ok(()) => println!("MMU: address-space self-test passed (ASID switch and user page)"),
        Err(reason) => fatal(&info, Some(platform), reason),
    }

    // Prove the recoverable-fault path on the installed map: an unmapped read,
    // a write to read-only text and an execute from writable data must all be
    // caught and resumed instead of halting the core.
    println!("BOOT STAGE: address-space self-test passed; checking fault recovery");
    match arch::aarch64::mmu::fault_self_test() {
        Ok(()) => println!(
            "MMU: fault self-test passed (unmapped read, read-only write, NX execute)"
        ),
        Err(reason) => fatal(&info, Some(platform), reason),
    }

    println!("BOOT STAGE: fault recovery passed; initializing heap");
    if let Err(reason) = heap::init() {
        fatal(&info, Some(platform), reason);
    }
    if let Err(reason) = selftest::heap_test() {
        fatal(&info, Some(platform), reason);
    }
        println!("BOOT STAGE: heap ready; probing devices");
        storage::probe();
        net::probe();
        drm::init();
        drm::report();
        partition::enumerate();
        partition::report();
        nvme::probe_and_report();
        let _ = fat32::Fat32::mount();

    // Discover every enabled MADT processor and install this core's per-core
    // pointer before the GIC or the scheduler needs it.
    println!("BOOT STAGE: device probes complete; discovering CPUs");
    if let Err(reason) = smp::discover(platform) {
        fatal(&info, Some(platform), reason);
    }
    topology::init(platform);

    println!("BOOT STAGE: CPU topology ready; initializing interrupt controller");
    if let Err(reason) = arch::aarch64::gic::init(platform) {
        fatal(&info, Some(platform), reason);
    }
    arch::aarch64::gic::report();

    println!("BOOT STAGE: interrupt controller ready; initializing timer");
    if let Err(reason) = arch::aarch64::timer::init() {
        fatal(&info, Some(platform), reason);
    }
    arch::aarch64::timer::report();

    // Publish the translation state and clean the kernel image, then start the
    // secondaries. Each secondary initializes its own GIC redistributor and
    // timer and reports online before waiting for the scheduler release.
    println!("BOOT STAGE: timer ready; preparing secondary CPUs");
    if let Err(reason) = smp::prepare(&info) {
        fatal(&info, Some(platform), reason);
    }
    println!("BOOT STAGE: starting secondary CPUs");
    smp::start_secondaries();
    smp::report();
    topology::report();

    match smp::run_ipi_test() {
        Ok(0) => println!("IPI: single-core run, no secondaries to test"),
        Ok(tested) => println!(
            "IPI: {} secondaries answered a ping/pong round trip",
            tested
        ),
        Err(reason) => fatal(&info, Some(platform), reason),
    }

    // Every core now runs in the higher half. Disable TTBR0 walks on the boot
    // core as well, leaving only the kernel half and the physical map.
    arch::aarch64::mmu::seal_low_half();
    println!("MMU: TTBR0 walks disabled; low half sealed");

    println!("BOOT STAGE: SMP online; initializing scheduler");
    if let Err(reason) = sched::init() {
        fatal(&info, Some(platform), reason);
    }
    sched::report();

    println!("BOOT STAGE: scheduler ready; releasing CPUs");
    smp::release_scheduler();
    sched::start()
}

/// Report a fatal pre-scheduler error and terminate cleanly under QEMU.
fn fatal(info: &BootInfo, platform: Option<&acpi::Platform>, reason: &str) -> ! {
    println!();
    println!("FATAL: {}", reason);
    if cfg!(feature = "smoke-test") && info.has_flag(FLAG_QEMU_VIRT) {
        if let Some(platform) = platform {
            if platform.psci.fadt_present && platform.psci.compliant {
                println!("QEMU: requesting PSCI SYSTEM_OFF");
                // SAFETY: the FADT declared PSCI and the conduit below.
                match unsafe { arch::aarch64::psci_system_off(platform.psci.use_hvc) } {
                    Ok(()) => println!("QEMU: PSCI SYSTEM_OFF returned unexpectedly"),
                    Err(code) => println!("QEMU: PSCI SYSTEM_OFF rejected (error {})", code),
                }
            }
        }
    }
    arch::halt()
}

fn report_firmware_devices(platform: &acpi::Platform) {
    if let Some(spcr) = &platform.spcr {
        println!();
        println!(
            "SPCR: interface 0x{:02x}, base 0x{:016x}, GSIV {}, baud code {}, parity {}, stop {}, flow 0x{:02x}, {}",
            spcr.interface,
            spcr.base,
            spcr.gsiv,
            spcr.baud,
            spcr.parity,
            spcr.stop_bits,
            spcr.flow_control,
            if spcr.base == 0 {
                "address absent"
            } else {
                "address present"
            }
        );
    }

    if platform.dbg2_ports != 0 {
        println!();
        println!("DBG2 debug devices: {}", platform.dbg2_ports);
        for port in platform.dbg2[..platform.dbg2_count].iter() {
            println!(
                "  {} type 0x{:04x} subtype 0x{:02x} base 0x{:016x}",
                ascii(&port.namespace[..port.namespace_length as usize]),
                port.port_type,
                port.subtype,
                port.base
            );
        }
    }

    if let Some(bgrt) = platform.bgrt {
        println!();
        println!(
            "BGRT: version {}.{}, status {}, type {}, image 0x{:016x} at ({}, {})",
            bgrt.version >> 8,
            bgrt.version & 0xff,
            bgrt.status,
            bgrt.image_type,
            bgrt.address,
            bgrt.offset_x,
            bgrt.offset_y
        );
    }

    if let Some(tpm) = platform.tpm2 {
        println!();
        println!(
            "TPM2: platform class 0x{:04x}, control area 0x{:016x}, start method {}",
            tpm.platform_class, tpm.control_area, tpm.start_method
        );
    }

    if platform.dsdt_address != 0 {
        println!();
        println!("DSDT: 0x{:016x} (AML evaluation is a later milestone)", platform.dsdt_address);
    }
}

fn ascii(bytes: &[u8]) -> &str {
    let end = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..end]).unwrap_or("<?>")
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

const _: () = assert!(core::mem::size_of::<BootInfo>() == 272);
