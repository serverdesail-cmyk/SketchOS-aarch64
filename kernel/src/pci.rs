//! PCIe host bridge inventory and ECAM enumeration.
//!
//! MCFG gives the kernel one or more ECAM windows. Each bus has 32 devices,
//! each device up to 8 functions, and each function 4 KiB of configuration
//! space. That layout is PCIe architecture, not a firmware guess.
//!
//! Enumeration is deliberately conservative:
//!
//! * It starts at the window's first bus and only follows secondary-bus
//!   numbers reported by PCI-to-PCI bridges. It never probes a bus the
//!   firmware did not describe or a bridge did not link.
//! * It reads only aligned 8/16/32-bit configuration registers. It does not
//!   write BARs to size them, because sizing requires temporarily disabling
//!   decoding and is not needed for an inventory.
//! * It runs only after the kernel owns page tables, so the ECAM window is
//!   mapped Device-nGnRnE rather than relying on the firmware map.

use crate::acpi::{Mcfg, Platform};
use crate::arch::aarch64::mmu;
use crate::print;
use crate::println;

/// One function exposes 4 KiB of configuration space, and every bus has
/// 32 devices with 8 functions each.
const CONFIG_BYTES_PER_BUS: u64 = 32 * 8 * 4096;
const MAX_BUSES: usize = 256;
const MAX_REPORTED_FUNCTIONS: usize = 64;

/// Summary of one decoded base address register.
#[derive(Clone, Copy)]
struct Bar {
    base: u64,
    is_io: bool,
    is_64: bool,
    prefetchable: bool,
}

/// Print the MCFG windows and, when the kernel owns the address space,
/// enumerate every reachable PCI function.
pub fn report(platform: &Platform) {
    println!();
    if platform.mcfg_count == 0 {
        println!("PCIe: no MCFG ECAM windows described");
        return;
    }

    println!("PCIe ECAM windows:");
    for window in platform.mcfg[..platform.mcfg_count].iter() {
        let buses = (window.end_bus as u64).saturating_sub(window.start_bus as u64) + 1;
        println!(
            "  segment {}: bus {:02x}-{:02x} ({} buses), ECAM 0x{:016x}, {} KiB",
            window.segment,
            window.start_bus,
            window.end_bus,
            buses,
            window.base,
            buses * CONFIG_BYTES_PER_BUS / 1024
        );
        println!(
            "    config address = 0x{:016x} + (bus - {:#04x}) << 20 + device << 15 + function << 12",
            window.base, window.start_bus
        );
    }

    if !mmu::is_enabled() {
        println!("  Enumeration skipped: kernel page tables are not active");
        return;
    }

    let mut totals = Totals::default();
    for window in platform.mcfg[..platform.mcfg_count].iter() {
        enumerate_window(window, &mut totals);
    }

    println!();
    println!(
        "PCIe: {} functions on {} buses across {} segments",
        totals.functions, totals.buses, totals.segments
    );
    println!(
        "  display {}, storage {}, network {}, USB {}, bridges {}, other {}",
        totals.display,
        totals.storage,
        totals.network,
        totals.usb,
        totals.bridges,
        totals.other
    );
    if totals.functions > MAX_REPORTED_FUNCTIONS {
        println!(
            "  (first {} of {} functions shown)",
            MAX_REPORTED_FUNCTIONS, totals.functions
        );
    }
}

#[derive(Default)]
struct Totals {
    functions: usize,
    buses: usize,
    segments: usize,
    display: usize,
    storage: usize,
    network: usize,
    usb: usize,
    bridges: usize,
    other: usize,
    reported: usize,
}

fn enumerate_window(window: &Mcfg, totals: &mut Totals) {
    if window.base == 0 || window.end_bus < window.start_bus {
        return;
    }

    let mut visited = [false; 256];
    let mut queue = [0u8; MAX_BUSES];
    let mut head = 0usize;
    let mut tail = 0usize;

    let first = window.start_bus;
    visited[first as usize] = true;
    queue[tail] = first;
    tail += 1;
    totals.segments += 1;

    while head < tail {
        let bus = queue[head];
        head += 1;
        totals.buses += 1;

        for device in 0..32u8 {
            let vendor = read16(window, bus, device, 0, 0x00);
            if vendor == 0xffff {
                continue;
            }

            let header = read8(window, bus, device, 0, 0x0e);
            let functions = if header & 0x80 != 0 { 8 } else { 1 };

            for function in 0..functions {
                if function != 0 {
                    let function_vendor = read16(window, bus, device, function, 0x00);
                    if function_vendor == 0xffff {
                        continue;
                    }
                }

                let vendor = read16(window, bus, device, function, 0x00);
                let device_id = read16(window, bus, device, function, 0x02);
                let class_revision = read32(window, bus, device, function, 0x08);
                let class = (class_revision >> 24) as u8;
                let subclass = (class_revision >> 16) as u8;
                let prog_if = (class_revision >> 8) as u8;
                let revision = class_revision as u8;
                let header = read8(window, bus, device, function, 0x0e) & 0x7f;

                totals.functions += 1;
                count_class(totals, class, subclass);

                if totals.reported < MAX_REPORTED_FUNCTIONS {
                    report_function(
                        window,
                        bus,
                        device,
                        function,
                        vendor,
                        device_id,
                        class,
                        subclass,
                        prog_if,
                        revision,
                        header,
                        totals,
                    );
                }

                if header == 0x01 || header == 0x02 {
                    let secondary = read8(window, bus, device, function, 0x19);
                    if secondary >= window.start_bus
                        && secondary <= window.end_bus
                        && !visited[secondary as usize]
                        && tail < MAX_BUSES
                    {
                        visited[secondary as usize] = true;
                        queue[tail] = secondary;
                        tail += 1;
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn report_function(
    window: &Mcfg,
    bus: u8,
    device: u8,
    function: u8,
    vendor: u16,
    device_id: u16,
    class: u8,
    subclass: u8,
    prog_if: u8,
    revision: u8,
    header: u8,
    totals: &mut Totals,
) {
    let category = class_name(class);
    let bridge = if header == 0x01 { " bridge" } else { "" };

    print!(
        "  pci:{:04x}:{:02x}:{:02x}.{} {:04x}:{:04x} {} {}{}",
        window.segment,
        bus,
        device,
        function,
        vendor,
        device_id,
        category,
        class_subclass_name(class, subclass, prog_if),
        bridge
    );
    if prog_if != 0 || revision != 0 {
        print!(" if {:02x} rev {:02x}", prog_if, revision);
    }

    if class == 0x01 || class == 0x02 || class == 0x03 || (class == 0x0c && subclass == 0x03) {
        if let Some(bar) = first_bar(window, bus, device, function) {
            print!(
                " bar 0x{:016x}{}{}",
                bar.base,
                if bar.is_io { " io" } else { "" },
                if bar.is_64 { " 64-bit" } else { "" }
            );
            if bar.prefetchable {
                print!(" prefetch");
            }
        }
    }

    println!();
    totals.reported += 1;
}

fn count_class(totals: &mut Totals, class: u8, subclass: u8) {
    match class {
        0x01 => totals.storage += 1,
        0x02 => totals.network += 1,
        0x03 => totals.display += 1,
        0x06 => totals.bridges += 1,
        0x0c if subclass == 0x03 => totals.usb += 1,
        _ => totals.other += 1,
    }
}

fn first_bar(window: &Mcfg, bus: u8, device: u8, function: u8) -> Option<Bar> {
    let bar = read32(window, bus, device, function, 0x10);
    if bar == 0 || bar == 0xffff_ffff {
        return None;
    }

    if bar & 1 != 0 {
        return Some(Bar {
            base: (bar & !0x3) as u64,
            is_io: true,
            is_64: false,
            prefetchable: false,
        });
    }

    let kind = (bar >> 1) & 0x3;
    let prefetchable = bar & 0x8 != 0;
    match kind {
        0x0 | 0x1 => Some(Bar {
            base: (bar & !0xf) as u64,
            is_io: false,
            is_64: false,
            prefetchable,
        }),
        0x2 => {
            let upper = read32(window, bus, device, function, 0x14);
            Some(Bar {
                base: ((bar & !0xf) as u64) | ((upper as u64) << 32),
                is_io: false,
                is_64: true,
                prefetchable,
            })
        }
        _ => None,
    }
}

fn config_address(
    window: &Mcfg,
    bus: u8,
    device: u8,
    function: u8,
    offset: u16,
) -> Option<u64> {
    if bus < window.start_bus || bus > window.end_bus || device >= 32 || function >= 8 {
        return None;
    }
    let bus_offset = (bus - window.start_bus) as u64;
    window
        .base
        .checked_add(bus_offset << 20)?
        .checked_add((device as u64) << 15)?
        .checked_add((function as u64) << 12)?
        .checked_add((offset & 0xfff) as u64)
}

fn read8(window: &Mcfg, bus: u8, device: u8, function: u8, offset: u16) -> u8 {
    match config_address(window, bus, device, function, offset) {
        // SAFETY: the address is inside an MCFG window that the MMU mapped
        // Device-nGnRnE through the physmap, and the offset is naturally
        // aligned for the width.
        Some(address) => unsafe {
            core::ptr::read_volatile(sketchos_bootinfo::phys_to_virt(address) as *const u8)
        },
        None => 0xff,
    }
}

fn read16(window: &Mcfg, bus: u8, device: u8, function: u8, offset: u16) -> u16 {
    match config_address(window, bus, device, function, offset) {
        // SAFETY: see `read8`.
        Some(address) => unsafe {
            core::ptr::read_volatile(sketchos_bootinfo::phys_to_virt(address) as *const u16)
        },
        None => 0xffff,
    }
}

fn read32(window: &Mcfg, bus: u8, device: u8, function: u8, offset: u16) -> u32 {
    match config_address(window, bus, device, function, offset) {
        // SAFETY: see `read8`.
        Some(address) => unsafe {
            core::ptr::read_volatile(sketchos_bootinfo::phys_to_virt(address) as *const u32)
        },
        None => 0xffff_ffff,
    }
}

fn class_name(class: u8) -> &'static str {
    match class {
        0x00 => "unclassified",
        0x01 => "storage",
        0x02 => "network",
        0x03 => "display",
        0x04 => "multimedia",
        0x05 => "memory",
        0x06 => "bridge",
        0x07 => "simple-comm",
        0x08 => "base-system",
        0x09 => "input",
        0x0a => "docking",
        0x0b => "processor",
        0x0c => "serial-bus",
        0x0d => "wireless",
        0x0e => "intelligent-io",
        0x0f => "satellite",
        0x10 => "encryption",
        0x11 => "signal-processing",
        0x12 => "accelerator",
        0x13 => "non-essential",
        0x40 => "co-processor",
        _ => "other",
    }
}

fn class_subclass_name(class: u8, subclass: u8, prog_if: u8) -> &'static str {
    match (class, subclass, prog_if) {
        (0x0c, 0x03, 0x30) => "xHCI USB host controller",
        (0x0c, 0x03, 0x20) => "EHCI USB host controller",
        (0x0c, 0x03, 0x10) => "OHCI USB host controller",
        (0x0c, 0x03, 0x00) => "UHCI USB host controller",
        (0x01, 0x06, _) => "SATA",
        (0x01, 0x07, _) => "SAS",
        (0x01, 0x08, _) => "NVMe",
        (0x02, 0x00, _) => "ethernet",
        (0x02, 0x80, _) => "other-network",
        (0x03, 0x00, _) => "VGA",
        (0x03, 0x80, _) => "other-display",
        (0x06, 0x00, _) => "host-bridge",
        (0x06, 0x01, _) => "ISA-bridge",
        (0x06, 0x04, _) => "PCI-bridge",
        (0x06, 0x09, _) => "PCIe-root-port",
        (0x0c, 0x03, _) => "USB host controller",
        (0x0d, 0x11, _) => "Bluetooth",
        (_, _, _) => "device",
    }
}
