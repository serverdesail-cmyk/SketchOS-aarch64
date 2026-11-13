//! CPU and interrupt-controller inventory.
//!
//! Reads only what MADT, GTDT and FADT stated. Nothing here starts a core,
//! programs the GIC, or enables a timer.

use crate::acpi::Platform;
use crate::println;

pub fn report(platform: &Platform) {
    println!();
    println!(
        "CPUs:"
    );
    println!(
        "  ACPI CPU interfaces: {} total, {} enabled",
        platform.cpu_count, platform.enabled_cpus
    );

    let mut efficiency_classes = [false; 256];
    for cpu in platform.cpus[..platform.cpu_count].iter() {
        efficiency_classes[cpu.efficiency as usize] = true;
        println!(
            "   iface {} uid {} MPIDR 0x{:016x}{}",
            cpu.interface,
            cpu.uid,
            cpu.mpidr,
            if cpu.enabled { "" } else { " (disabled)" }
        );
        if cpu.gicr != 0 {
            println!("       GICR 0x{:016x}", cpu.gicr);
        }
        if cpu.gicc != 0 || cpu.gicv != 0 || cpu.gich != 0 {
            println!(
                "       GICC 0x{:016x}, GICV 0x{:016x}, GICH 0x{:016x}",
                cpu.gicc, cpu.gicv, cpu.gich
            );
        }
        println!("       efficiency {} class", cpu.efficiency);
        gsiv_line("PMU GSIV", cpu.performance_gsiv);
        gsiv_line("VGIC GSIV", cpu.vgic_gsiv);
    }

    let classes = efficiency_classes.iter().filter(|present| **present).count();
    println!("  Power efficiency classes present: {}", classes);

    println!();
    println!("Interrupt controllers:");
    match platform.gicd {
        Some(gicd) => {
            println!(
                "  GICD: id {}, base 0x{:016x}, version {}",
                gicd.id, gicd.base, gicd.version
            );
        }
        None => println!("  GICD: not described"),
    }
    if platform.gicr_count == 0 {
        println!("  GICR: not described");
    } else {
        for gicr in platform.gicr[..platform.gicr_count].iter() {
            println!(
                "  GICR: base 0x{:016x}, length 0x{:x} ({} KiB)",
                gicr.base,
                gicr.length,
                gicr.length / 1024
            );
        }
    }
    if platform.its_count == 0 {
        println!("  ITS: not described");
    } else {
        for its in platform.its[..platform.its_count].iter() {
            println!("  ITS: id {}, base 0x{:016x}", its.id, its.base);
        }
    }
    if platform.gic_msi_frames != 0 {
        println!("  GICv2m MSI frames: {}", platform.gic_msi_frames);
    }

    println!();
    println!("Timers and power:");
    match platform.timer {
        Some(timer) => {
            println!(
                "  GTDT: control 0x{:016x}, read 0x{:016x}, platform timers {}",
                timer.control_base, timer.read_base, timer.platform_timers
            );
            flash_gsiv("  EL1 secure GSIV", timer.secure_el1_gsiv, timer.secure_el1_flags);
            flash_gsiv(
                "  EL1 non-secure GSIV",
                timer.non_secure_el1_gsiv,
                timer.non_secure_el1_flags,
            );
            flash_gsiv("  EL1 virtual GSIV", timer.virtual_el1_gsiv, timer.virtual_el1_flags);
            flash_gsiv(
                "  EL2 non-secure GSIV",
                timer.non_secure_el2_gsiv,
                timer.non_secure_el2_flags,
            );
        }
        None => println!("  GTDT: not described"),
    }
    if platform.psci.fadt_present {
        println!(
            "  PSCI: compliant {}, conduit {}",
            yes_no(platform.psci.compliant),
            if !platform.psci.compliant {
                "unknown"
            } else if platform.psci.use_hvc {
                "HVC"
            } else {
                "SMC"
            }
        );
    } else {
        println!("  PSCI: FADT not described");
    }
}

fn gsiv_line(label: &str, value: u32) {
    if value == u32::MAX {
        println!("       {}: none", label);
    } else {
        println!("       {}: {}", label, value);
    }
}

fn flash_gsiv(label: &str, value: u32, flags: u32) {
    if value == u32::MAX {
        println!("{}, flags 0x{:x}", label, flags);
    } else {
        println!("{} {}, flags 0x{:x}", label, value, flags);
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}
