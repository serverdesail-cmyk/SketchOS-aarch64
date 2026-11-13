# Driver API

The shared driver vocabulary lives in the `sketchos-driver-api` crate
(`drivers/`). It is `no_std`, allocation-free, and independent of any bus or
architecture. The kernel uses it to describe devices; a future driver uses it to
match, claim and operate one.

## Design rules

1. **A driver is a plain Rust value.** The kernel owns it and calls its methods
   in a fixed order.
2. **All resource addresses are physical.** The kernel maps them before a driver
   may dereference them. A driver never guesses a register address and never
   touches a resource it was not given.
3. **Everything is fallible.** `probe` returns `Error`; a missing or unexpected
   device must not take down the machine.
4. **Unsafe stays local.** The generic API does not hide MMIO. Unsafe access
   belongs in a small, reviewed driver module.
5. **Matching does not touch hardware.** `matches` is called during
   enumeration and must only inspect the device description.

## Lifecycle

```text
matches(device) -> bool
        |
        v
probe(device)  -> Result<(), Error>     claim and initialise, interrupts off
        |
        v
start()        -> Result<(), Error>     enable the device
        |
        v
stop()                                  quiesce and release
```

The kernel guarantees this order for a successful probe. `stop` is also called
when `start` fails. `bind` only runs `matches` and `probe`; enabling is a
separate step so a caller can probe several devices before starting any.

## Identity

`DeviceId` has four forms:

- `Acpi { hid, uid }` — an ACPI namespace device. HIDs are 8 bytes and
  space-padded, so trailing spaces are meaningful.
- `Pci { address, ids }` — a PCI/PCIe function with segment, bus, device,
  function, vendor, device, class, subclass and programming interface.
- `Platform { kind, instance }` — a firmware-described device without a
  resolvable ACPI namespace ID, such as a UART from SPCR.
- `Virtual { kind, instance }` — a device created by the kernel.

Convenience constructors and predicates exist for the common cases:
`DeviceId::acpi4`, `acpi8`, `pci`, `platform`, `virtual_device`,
`Device::is_acpi`, `is_pci`, `is_platform`, `first_mmio`, `mmio`, `dma` and
`interrupts`.

## Resources

`Resource` describes what firmware assigned to a device:

| Variant | Meaning |
| --- | --- |
| `Mmio(MmioRegion)` | Memory-mapped I/O window, 64-bit and prefetchable flags |
| `Dma(DmaRegion)` | DMA-capable memory window, coherent flag |
| `Interrupt(Interrupt)` | SPI, PPI, SGI, MSI, MSI-X or unknown, with raw flags |
| `Clock(Clock)` | Reference clock ID and nominal Hz, or zero if unknown |
| `Power(u32)` | Power domain or power resource identifier |
| `Vendor { vendor, data }` | Opaque vendor payload the kernel passes through |

The kernel maps `Mmio` and `Dma` regions before handing them to a driver. A
driver must not assume a resource exists; `first_mmio()` and the iterators
return `Option`/empty iterators.

## Device classes

`DeviceClass` is a coarse reporting and arbitration label: `Processor`,
`Memory`, `InterruptController`, `Timer`, `Display`, `Gpu`, `Serial`,
`Storage`, `Network`, `Usb`, `Power`, `Input`, `Audio`, `Camera`, `Other`.
`DeviceClass::from_pci_base_class` maps PCI base class codes to this enum.

## Errors

`Error` is a small `Copy` enum with stable names for logs: `NoMatch`,
`Unsupported`, `BadResource`, `BusError`, `Timeout`, `OutOfMemory`,
`HardwareFault`, `AlreadyClaimed`, `InvalidArgument`, `NotImplemented`.

## Implementing a driver

```rust
use sketchos_driver_api::{Device, Driver, Error};

struct Pl011;

impl Driver for Pl011 {
    fn name(&self) -> &'static str { "pl011" }

    fn matches(&self, device: &Device<'_>) -> bool {
        device.is_platform("pl011")
    }

    fn probe(&mut self, device: &Device<'_>) -> Result<(), Error> {
        let _uart = device.first_mmio().ok_or(Error::BadResource)?;
        // Verify the PrimeCell ID through reviewed MMIO helpers, then map.
        Ok(())
    }

    fn start(&mut self) -> Result<(), Error> {
        // Enable the UART and its interrupt only now.
        Ok(())
    }

    fn stop(&mut self) {
        // Quiesce. Must not panic.
    }
}
```

`bind(&mut [&mut dyn Driver], &Device)` returns the first binding whose
`matches` succeeds. `match_count` reports how many drivers claim a device
without probing it.

## Current state

No driver is bound in milestone 0.0.6. The kernel builds `Device` records for
processors, the GIC distributor/redistributor/ITS, the generic timer, the SPCR
UART, DBG2 debug ports, PCIe ECAM host bridges and the BGRT image, and prints
the vocabulary. It does not run a driver registry or call `probe`.

The first real driver is expected to be the QEMU PL011 (already used directly by
the console) or a documented laptop UART. Storage, GPU, network and input
drivers remain future work.
