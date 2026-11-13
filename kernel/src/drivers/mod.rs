//! Driver-facing device inventory.
//!
//! Every item the firmware described is presented through the same
//! `sketchos-driver-api` vocabulary a real driver will see. No driver is
//! bound yet; this milestone proves the vocabulary is expressive enough for
//! the hardware the laptop actually reports.

use crate::acpi::Platform;
use crate::print;
use crate::println;
use sketchos_driver_api::{
    Bus, Device, DeviceClass, DeviceId, Interrupt, InterruptKind, MmioRegion, Resource,
};

const CLASSES: [DeviceClass; 15] = [
    DeviceClass::Processor,
    DeviceClass::Memory,
    DeviceClass::InterruptController,
    DeviceClass::Timer,
    DeviceClass::Display,
    DeviceClass::Gpu,
    DeviceClass::Serial,
    DeviceClass::Storage,
    DeviceClass::Network,
    DeviceClass::Usb,
    DeviceClass::Power,
    DeviceClass::Input,
    DeviceClass::Audio,
    DeviceClass::Camera,
    DeviceClass::Other,
];

pub fn report(platform: &Platform) {
    let mut counts = [0usize; 15];

    println!();
    println!("Driver API devices:");

    for cpu in platform.cpus[..platform.cpu_count].iter() {
        if !cpu.enabled {
            continue;
        }
        show(
            Device {
                id: DeviceId::platform("cpu", cpu.uid),
                bus: Bus::Platform,
                class: DeviceClass::Processor,
                name: "ACPI processor",
                resources: &[],
            },
            &mut counts,
        );
    }

    if let Some(gicd) = platform.gicd {
        let resources = [Resource::Mmio(MmioRegion {
            base: gicd.base,
            length: 0,
            supports_64_bit: true,
            prefetchable: false,
        })];
        show(
            Device {
                id: DeviceId::platform("gicd", gicd.id),
                bus: Bus::Platform,
                class: DeviceClass::InterruptController,
                name: "GIC distributor",
                resources: &resources,
            },
            &mut counts,
        );
    }

    for (index, gicr) in platform.gicr[..platform.gicr_count].iter().enumerate() {
        let resources = [Resource::Mmio(MmioRegion {
            base: gicr.base,
            length: gicr.length as u64,
            supports_64_bit: true,
            prefetchable: false,
        })];
        show(
            Device {
                id: DeviceId::platform("gicr", index as u32),
                bus: Bus::Platform,
                class: DeviceClass::InterruptController,
                name: "GIC redistributor range",
                resources: &resources,
            },
            &mut counts,
        );
    }

    for its in platform.its[..platform.its_count].iter() {
        let resources = [Resource::Mmio(MmioRegion {
            base: its.base,
            length: 0,
            supports_64_bit: true,
            prefetchable: false,
        })];
        show(
            Device {
                id: DeviceId::platform("gic-its", its.id),
                bus: Bus::Platform,
                class: DeviceClass::InterruptController,
                name: "GIC Interrupt Translation Service",
                resources: &resources,
            },
            &mut counts,
        );
    }

    if platform.timer.is_some() {
        show(
            Device {
                id: DeviceId::platform("generic-timer", 0),
                bus: Bus::Platform,
                class: DeviceClass::Timer,
                name: "ARM generic timer",
                resources: &[],
            },
            &mut counts,
        );
    }

    if let Some(spcr) = &platform.spcr {
        let resources = [
            Resource::Mmio(MmioRegion {
                base: spcr.base,
                length: 0,
                supports_64_bit: true,
                prefetchable: false,
            }),
            Resource::Interrupt(Interrupt {
                kind: if spcr.interrupt_type & 0x08 != 0 {
                    InterruptKind::Spi
                } else {
                    InterruptKind::Unknown
                },
                number: spcr.gsiv,
                flags: 0,
            }),
        ];
        show(
            Device {
                id: DeviceId::platform("spcr-uart", spcr.interface as u32),
                bus: Bus::Platform,
                class: DeviceClass::Serial,
                name: "SPCR debug UART",
                resources: &resources,
            },
            &mut counts,
        );
    }

    for (index, port) in platform.dbg2[..platform.dbg2_count].iter().enumerate() {
        let resources = [Resource::Mmio(MmioRegion {
            base: port.base,
            length: 0,
            supports_64_bit: true,
            prefetchable: false,
        })];
        show(
            Device {
                id: DeviceId::platform("dbg2-debug-port", index as u32),
                bus: Bus::Platform,
                class: DeviceClass::Serial,
                name: "DBG2 debug device",
                resources: &resources,
            },
            &mut counts,
        );
    }

    for window in platform.mcfg[..platform.mcfg_count].iter() {
        let buses = (window.end_bus as u64).saturating_sub(window.start_bus as u64) + 1;
        let resources = [Resource::Mmio(MmioRegion {
            base: window.base,
            // One MiB per bus: 32 devices x 8 functions x 4 KiB.
            length: buses * 1024 * 1024,
            supports_64_bit: true,
            prefetchable: false,
        })];
        show(
            Device {
                id: DeviceId::platform("pcie-ecam", window.segment as u32),
                bus: Bus::Pci,
                class: DeviceClass::Other,
                name: "PCIe ECAM host bridge",
                resources: &resources,
            },
            &mut counts,
        );
    }

    if let Some(bgrt) = platform.bgrt {
        let resources = [Resource::Mmio(MmioRegion {
            base: bgrt.address,
            length: 0,
            supports_64_bit: true,
            prefetchable: false,
        })];
        show(
            Device {
                id: DeviceId::platform("boot-graphics", 0),
                bus: Bus::Platform,
                class: DeviceClass::Display,
                name: "BGRT boot image",
                resources: &resources,
            },
            &mut counts,
        );
    }

    if let Some(tpm) = platform.tpm2 {
        let resources = [Resource::Mmio(MmioRegion {
            base: tpm.control_area,
            length: 0,
            supports_64_bit: true,
            prefetchable: false,
        })];
        show(
            Device {
                id: DeviceId::platform("tpm2", tpm.platform_class as u32),
                bus: Bus::Platform,
                class: DeviceClass::Other,
                name: "TPM 2.0",
                resources: &resources,
            },
            &mut counts,
        );
    }

    println!("Device classes:");
    let mut total = 0usize;
    for (index, class) in CLASSES.iter().enumerate() {
        if counts[index] != 0 {
            println!("  {:<20} {}", class.name(), counts[index]);
            total += counts[index];
        }
    }
    println!("  {:<20} {}", "total", total);
}

fn show(device: Device<'_>, counts: &mut [usize; 15]) {
    counts[class_index(device.class)] += 1;

    print!("  [{:<20}] ", device.class.name());
    match device.id {
        DeviceId::Acpi { hid, uid } => {
            print!("acpi:{}, uid {}", ascii(&hid), uid);
        }
        DeviceId::Pci { address, ids } => {
            print!(
                "pci:{:04x}:{:02x}:{:02x}.{} {:04x}:{:04x}",
                address.segment,
                address.bus,
                address.device,
                address.function,
                ids.vendor,
                ids.device
            );
        }
        DeviceId::Platform { kind, instance } | DeviceId::Virtual { kind, instance } => {
            print!("{}#{}", kind, instance);
        }
    }
    println!(" | {}", device.name);
}

fn class_index(class: DeviceClass) -> usize {
    match class {
        DeviceClass::Processor => 0,
        DeviceClass::Memory => 1,
        DeviceClass::InterruptController => 2,
        DeviceClass::Timer => 3,
        DeviceClass::Display => 4,
        DeviceClass::Gpu => 5,
        DeviceClass::Serial => 6,
        DeviceClass::Storage => 7,
        DeviceClass::Network => 8,
        DeviceClass::Usb => 9,
        DeviceClass::Power => 10,
        DeviceClass::Input => 11,
        DeviceClass::Audio => 12,
        DeviceClass::Camera => 13,
        DeviceClass::Other => 14,
    }
}

fn ascii(bytes: &[u8]) -> &str {
    let end = bytes
        .iter()
        .position(|&byte| byte == 0 || byte == b' ')
        .unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..end]).unwrap_or("?")
}
