//! SketchOS driver API.
//!
//! This crate defines the vocabulary a driver and the kernel share:
//! device identities, firmware-provided resources, device classes and the
//! [`Driver`] lifecycle. It is `no_std`, allocation-free and deliberately
//! independent of any particular bus.
//!
//! # Design rules
//!
//! * A driver is a plain Rust value. The kernel owns it and calls [`Driver`]
//!   methods in a fixed order: `matches`, `probe`, `start`, then eventually
//!   `stop`.
//! * All addresses in [`Resource`] are physical addresses. The kernel maps
//!   them before a driver may dereference them. A driver never guesses a
//!   register address and never touches a resource it was not given.
//! * Everything is fallible. `probe` returns [`Error`] instead of panicking,
//!   so a missing or unexpected device cannot take down the machine.
//! * Nothing in this crate hides unsafe operations. MMIO access belongs in a
//!   small, reviewed driver module, not in the generic API.
//!
//! # Example
//!
//! ```ignore
//! use sketchos_driver_api::{Device, Driver, Error};
//!
//! struct Pl011;
//!
//! impl Driver for Pl011 {
//!     fn name(&self) -> &'static str { "pl011" }
//!
//!     fn matches(&self, device: &Device<'_>) -> bool {
//!         device.is_platform("pl011")
//!     }
//!
//!     fn probe(&mut self, device: &Device<'_>) -> Result<(), Error> {
//!         let uart = device.first_mmio().ok_or(Error::BadResource)?;
//!         // Verify the PrimeCell ID via the kernel MMIO helpers, then activate.
//!         let _ = uart;
//!         Ok(())
//!     }
//! }
//! ```

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

#[cfg(test)]
extern crate std;

/// Maximum characters in an ACPI hardware ID, including the NUL-free padding.
pub const ACPI_HID_LEN: usize = 8;

/// A compact, allocation-free device identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceId {
    /// ACPI namespace device. `hid` is the 8-byte hardware ID as stored by
    /// firmware; trailing spaces are meaningful.
    Acpi {
        /// Hardware ID, for example `*b"ACPI0007"` or `*b"QCOM0D17"`.
        hid: [u8; ACPI_HID_LEN],
        /// `_UID` value, or zero when the device has no UID.
        uid: u32,
    },
    /// PCI/PCIe function.
    Pci {
        /// Bus/device/function address, including the ECAM segment.
        address: PciAddress,
        /// Vendor and device identifiers plus class information.
        ids: PciId,
    },
    /// A firmware-described platform device that has no ACPI namespace
    /// identifier the kernel can currently resolve (for example a UART from
    /// SPCR). `kind` is a stable, lowercase name owned by the kernel.
    Platform {
        /// Stable short name, such as `"pl011"` or `"gic-v3"`.
        kind: &'static str,
        /// Instance number for repeated devices.
        instance: u32,
    },
    /// A purely virtual device created by a kernel subsystem.
    Virtual {
        /// Stable short name.
        kind: &'static str,
        /// Instance number.
        instance: u32,
    },
}

impl DeviceId {
    /// Build an ACPI device identifier from a 4-character HID and an
    /// optional numeric UID. Short HIDs are space padded, matching ACPI.
    pub const fn acpi4(hid: [u8; 4], uid: u32) -> Self {
        let mut padded = [b' '; ACPI_HID_LEN];
        padded[0] = hid[0];
        padded[1] = hid[1];
        padded[2] = hid[2];
        padded[3] = hid[3];
        Self::Acpi { hid: padded, uid }
    }

    /// Build an ACPI device identifier from an 8-byte HID.
    pub const fn acpi8(hid: [u8; ACPI_HID_LEN], uid: u32) -> Self {
        Self::Acpi { hid, uid }
    }

    /// Build a PCI function identifier.
    pub const fn pci(address: PciAddress, ids: PciId) -> Self {
        Self::Pci { address, ids }
    }

    /// Build a platform device identifier.
    pub const fn platform(kind: &'static str, instance: u32) -> Self {
        Self::Platform { kind, instance }
    }

    /// Build a virtual device identifier.
    pub const fn virtual_device(kind: &'static str, instance: u32) -> Self {
        Self::Virtual { kind, instance }
    }

    /// ACPI HID, if this is an ACPI device.
    pub const fn acpi_hid(&self) -> Option<[u8; ACPI_HID_LEN]> {
        match self {
            Self::Acpi { hid, .. } => Some(*hid),
            _ => None,
        }
    }

    /// PCI identity, if this is a PCI function.
    pub const fn pci_ids(&self) -> Option<PciId> {
        match self {
            Self::Pci { ids, .. } => Some(*ids),
            _ => None,
        }
    }

    /// Platform kind, if this is a platform or virtual device.
    pub const fn platform_kind(&self) -> Option<&'static str> {
        match self {
            Self::Platform { kind, .. } | Self::Virtual { kind, .. } => Some(kind),
            _ => None,
        }
    }
}

/// PCI bus/device/function address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciAddress {
    /// ECAM segment group.
    pub segment: u16,
    /// Bus number, 0..=255.
    pub bus: u8,
    /// Device number, 0..=31.
    pub device: u8,
    /// Function number, 0..=7.
    pub function: u8,
}

/// PCI vendor/device/class identifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciId {
    /// Vendor ID, for example `0x1af4`.
    pub vendor: u16,
    /// Device ID.
    pub device: u16,
    /// Base class code, for example `0x03` for a display controller.
    pub class: u8,
    /// Subclass code.
    pub subclass: u8,
    /// Programming interface.
    pub prog_if: u8,
}

/// Bus a device was discovered on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bus {
    /// ACPI namespace or an ACPI-described platform device.
    Acpi,
    /// PCI/PCIe configuration space.
    Pci,
    /// Firmware-described platform device without an ACPI namespace entry.
    Platform,
    /// Created by the kernel.
    Virtual,
}

/// Coarse device class used for reporting and driver arbitration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceClass {
    /// Application processor.
    Processor,
    /// Memory controller, cache or RAM device.
    Memory,
    /// Interrupt controller (GIC, ITS, MSI controller).
    InterruptController,
    /// System timer.
    Timer,
    /// Display controller or framebuffer.
    Display,
    /// General-purpose GPU or accelerator.
    Gpu,
    /// Serial controller.
    Serial,
    /// Storage controller.
    Storage,
    /// Network controller.
    Network,
    /// USB controller.
    Usb,
    /// Power, clock or thermal management.
    Power,
    /// Input device.
    Input,
    /// Audio device.
    Audio,
    /// Camera or image signal processor.
    Camera,
    /// Anything the kernel has not classified yet.
    Other,
}

impl DeviceClass {
    /// Short lowercase name for diagnostics.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Processor => "processor",
            Self::Memory => "memory",
            Self::InterruptController => "interrupt-controller",
            Self::Timer => "timer",
            Self::Display => "display",
            Self::Gpu => "gpu",
            Self::Serial => "serial",
            Self::Storage => "storage",
            Self::Network => "network",
            Self::Usb => "usb",
            Self::Power => "power",
            Self::Input => "input",
            Self::Audio => "audio",
            Self::Camera => "camera",
            Self::Other => "other",
        }
    }

    /// Classify a PCI base class code.
    pub const fn from_pci_base_class(class: u8) -> Self {
        match class {
            0x01 => Self::Storage,
            0x02 => Self::Network,
            0x03 => Self::Display,
            0x04 => Self::Other, // multimedia
            0x06 => Self::Other, // bridge
            0x0c => Self::Serial, // serial bus (USB, SMBus, ...)
            _ => Self::Other,
        }
    }
}

/// One MMIO window assigned to a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MmioRegion {
    /// Physical base address.
    pub base: u64,
    /// Window length in bytes.
    pub length: u64,
    /// True when the region may be accessed with 64-bit loads/stores.
    pub supports_64_bit: bool,
    /// True when the region is prefetchable.
    pub prefetchable: bool,
}

/// One DMA-capable memory window assigned to a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaRegion {
    /// Physical base address of the window.
    pub base: u64,
    /// Window length in bytes.
    pub length: u64,
    /// True when the device is coherent with the CPU caches.
    pub coherent: bool,
}

/// Interrupt source kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterruptKind {
    /// GIC shared peripheral interrupt (SPI).
    Spi,
    /// GIC private peripheral interrupt (PPI).
    Ppi,
    /// GIC software-generated interrupt (SGI).
    Sgi,
    /// Message-signalled interrupt.
    Msi,
    /// Extended message-signalled interrupt.
    MsiX,
    /// Wired interrupt the firmware did not classify.
    Unknown,
}

/// One interrupt source assigned to a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interrupt {
    /// Interrupt kind.
    pub kind: InterruptKind,
    /// Interrupt number, as used by the interrupt controller.
    pub number: u32,
    /// Firmware trigger/polarity flags, unmodified.
    pub flags: u32,
}

/// One reference clock or timer input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clock {
    /// Firmware clock identifier.
    pub id: u32,
    /// Nominal frequency in Hz, or zero when firmware did not state it.
    pub hz: u64,
}

/// A resource assigned to a device by firmware.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resource {
    /// Memory-mapped I/O window.
    Mmio(MmioRegion),
    /// DMA-capable memory window.
    Dma(DmaRegion),
    /// Interrupt source.
    Interrupt(Interrupt),
    /// Reference clock.
    Clock(Clock),
    /// Power domain or power resource identifier.
    Power(u32),
    /// Vendor-specific resource bytes. The driver and firmware must agree on
    /// the meaning; the kernel only passes it through.
    Vendor {
        /// Vendor identifier, such as a PCI vendor ID or ACPI HID prefix.
        vendor: u32,
        /// Raw resource payload.
        data: &'static [u8],
    },
}

/// Classification of a discovered device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Device<'a> {
    /// Stable identity used for matching and reporting.
    pub id: DeviceId,
    /// Bus the device was discovered on.
    pub bus: Bus,
    /// Coarse class for reporting.
    pub class: DeviceClass,
    /// Human-readable name, or an empty string when firmware provided none.
    pub name: &'static str,
    /// Firmware-assigned resources.
    pub resources: &'a [Resource],
}

impl<'a> Device<'a> {
    /// Iterate over MMIO windows in firmware order.
    pub fn mmio(&self) -> impl Iterator<Item = &MmioRegion> + '_ {
        self.resources.iter().filter_map(|resource| match resource {
            Resource::Mmio(region) => Some(region),
            _ => None,
        })
    }

    /// Iterate over DMA windows in firmware order.
    pub fn dma(&self) -> impl Iterator<Item = &DmaRegion> + '_ {
        self.resources.iter().filter_map(|resource| match resource {
            Resource::Dma(region) => Some(region),
            _ => None,
        })
    }

    /// Iterate over interrupt sources in firmware order.
    pub fn interrupts(&self) -> impl Iterator<Item = &Interrupt> + '_ {
        self.resources.iter().filter_map(|resource| match resource {
            Resource::Interrupt(interrupt) => Some(interrupt),
            _ => None,
        })
    }

    /// First MMIO window, if any.
    pub fn first_mmio(&self) -> Option<&MmioRegion> {
        self.mmio().next()
    }

    /// True when this is the ACPI device with `hid` and any UID.
    pub fn is_acpi(&self, hid: [u8; ACPI_HID_LEN]) -> bool {
        self.id.acpi_hid() == Some(hid)
    }

    /// True when this is a platform device of `kind`.
    pub fn is_platform(&self, kind: &str) -> bool {
        self.id.platform_kind() == Some(kind)
    }

    /// True when this is a PCI function with the given vendor/device pair.
    pub fn is_pci(&self, vendor: u16, device: u16) -> bool {
        match self.id.pci_ids() {
            Some(ids) => ids.vendor == vendor && ids.device == device,
            None => false,
        }
    }
}

/// Driver failure modes. No variant carries an allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The device is not supported by this driver.
    NoMatch,
    /// The device is recognised but this build has no support for it.
    Unsupported,
    /// A required firmware resource is missing or malformed.
    BadResource,
    /// A bus transaction failed.
    BusError,
    /// A hardware operation did not complete in time.
    Timeout,
    /// The kernel could not provide memory or address space.
    OutOfMemory,
    /// The device reported an unrecoverable hardware fault.
    HardwareFault,
    /// Another driver already owns the device.
    AlreadyClaimed,
    /// A caller-supplied argument is invalid.
    InvalidArgument,
    /// The operation is not implemented yet.
    NotImplemented,
}

impl Error {
    /// Short stable name for logs.
    pub const fn name(self) -> &'static str {
        match self {
            Self::NoMatch => "no-match",
            Self::Unsupported => "unsupported",
            Self::BadResource => "bad-resource",
            Self::BusError => "bus-error",
            Self::Timeout => "timeout",
            Self::OutOfMemory => "out-of-memory",
            Self::HardwareFault => "hardware-fault",
            Self::AlreadyClaimed => "already-claimed",
            Self::InvalidArgument => "invalid-argument",
            Self::NotImplemented => "not-implemented",
        }
    }
}

/// Driver lifecycle.
///
/// The kernel guarantees this order for a successful probe:
/// `matches` -> `probe` -> `start` -> ... -> `stop`.
/// `stop` is also called when `start` fails.
pub trait Driver {
    /// Stable driver name used in logs and bindings.
    fn name(&self) -> &'static str;

    /// Return true when this driver can handle `device`. This must not touch
    /// hardware; it is called during enumeration.
    fn matches(&self, device: &Device<'_>) -> bool;

    /// Claim and initialise the device. `probe` may map resources but must
    /// leave interrupts disabled until `start`.
    fn probe(&mut self, device: &Device<'_>) -> Result<(), Error>;

    /// Enable the device. Called after a successful `probe`.
    fn start(&mut self) -> Result<(), Error> {
        Ok(())
    }

    /// Quiesce the device and release ownership. Must be safe to call after a
    /// failed `start` and must not panic.
    fn stop(&mut self) {}
}

/// Result of binding one driver to one device, without allocating.
#[derive(Clone, Copy, Debug)]
pub struct Binding {
    /// Driver that claimed the device.
    pub driver: &'static str,
    /// Probe result.
    pub result: Result<(), Error>,
}

/// Try every driver in `drivers` against `device` and return the first
/// successful binding. Drivers whose `matches` returns false are skipped.
///
/// `start` is intentionally a separate call: a caller may probe several
/// devices before enabling any of them.
pub fn bind<'a>(
    drivers: &mut [&mut dyn Driver],
    device: &Device<'a>,
) -> Option<Binding> {
    for driver in drivers.iter_mut() {
        if !driver.matches(device) {
            continue;
        }
        let name = driver.name();
        return Some(Binding {
            driver: name,
            result: driver.probe(device),
        });
    }
    None
}

/// Count how many drivers claim a device without probing it.
pub fn match_count(drivers: &[&mut dyn Driver], device: &Device<'_>) -> usize {
    drivers
        .iter()
        .filter(|driver| driver.matches(device))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acpi_hid_is_space_padded() {
        let id = DeviceId::acpi4(*b"ACPI", 0);
        assert_eq!(id.acpi_hid(), Some(*b"ACPI    "));
    }

    #[test]
    fn pci_matching() {
        let id = DeviceId::pci(
            PciAddress {
                segment: 0,
                bus: 1,
                device: 2,
                function: 3,
            },
            PciId {
                vendor: 0x1af4,
                device: 0x1050,
                class: 0x03,
                subclass: 0x00,
                prog_if: 0x00,
            },
        );
        let device = Device {
            id,
            bus: Bus::Pci,
            class: DeviceClass::Display,
            name: "",
            resources: &[],
        };
        assert!(device.is_pci(0x1af4, 0x1050));
        assert!(!device.is_pci(0x1af4, 0x0001));
    }

    #[test]
    fn resource_iterators_are_typed() {
        let resources = [
            Resource::Mmio(MmioRegion {
                base: 0x1000,
                length: 0x100,
                supports_64_bit: false,
                prefetchable: false,
            }),
            Resource::Interrupt(Interrupt {
                kind: InterruptKind::Spi,
                number: 42,
                flags: 0,
            }),
        ];
        let device = Device {
            id: DeviceId::platform("test", 0),
            bus: Bus::Platform,
            class: DeviceClass::Other,
            name: "test",
            resources: &resources,
        };
        assert_eq!(device.mmio().count(), 1);
        assert_eq!(device.interrupts().count(), 1);
        assert!(device.first_mmio().is_some());
    }
}
