# AML runtime decision for the hardware kernel

Date: 2026-11-13  
Target: HP OmniBook X 14 / Snapdragon X Plus X1P-42-100  
Status: design decision; no AML interpreter has been integrated yet.

## Why this is mandatory

The current ACPI code decodes static tables (MADT, GTDT, MCFG, IORT, SPCR, DBG2, etc.) but does not evaluate the DSDT/SSDT namespace. That is not enough to bring up laptop platform devices. The keyboard, touchpad, GPIO, QUP/I2C and power resources are described by ACPI device objects and methods such as:

- `_HID`, `_CID`, `_UID`, `_ADR`, `_STA`
- `_CRS`, `_PRS`, `_SRS`
- `_DEP`, `_INI`, `_PS0`, `_PS3`
- GPIO and I2C SerialBus resource descriptors
- OperationRegion fields that access SystemMemory, GPIO, I2C, PCI_Config and other address spaces

Hard-coding guessed MMIO addresses or device names without evaluating these resources would be unsafe.

## Candidate evaluation

### Preferred: uACPI 7.0.0

Pin the upstream release, not a moving branch:

- Release: `7.0.0`, published 2026-10-16
- Tag commit: `686a9a52d9ad72690406d7cada82801377d5e2fa`
- License: MIT
- Upstream repository: https://github.com/uACPI/uACPI
- Release notes: https://github.com/uACPI/uACPI/releases/tag/7.0.0

Reasons:
- The project provides an AML interpreter, namespace manager, ACPI resource subsystem, operation-region handlers, device search, PCI routing, events and notifications.
- Its README states support for ACPI 6.6 resource descriptors and 32-/64-bit platforms.
- The interpreter is designed to avoid recursion and reduce kernel stack use.
- The project documents a freestanding kernel integration API and a compile-time reduced-hardware option.
- Version 7.0.0 simplifies the host API to counting semaphores and preallocated work items, and its release notes list fixes for AML compatibility, concurrency and GPE lifetime bugs.

These are upstream project claims; we still need to pin the source, review the exact release, compile it for AArch64 and test it against the laptop's actual AML before enabling device methods.

### Not selected unmodified: rust-osdev/acpi 6.1.1

This is a pure-Rust, no_std-capable ACPI library with an allocator-backed AML interpreter. It is attractive for language consistency, but the current source still lists important interpreter TODOs including loop timeouts and panic-free/fuzzed interpretation. Its namespace initialization path also contains a panic for an unexpected `_STA` return type.

We should not put that interpreter in control of real laptop power/GPIO/I2C methods without first fixing and testing those paths. It remains a possible source for static table parsing or a future Rust interpreter if those issues are addressed.

### Reference/fallback: ACPICA

ACPICA is the mature reference implementation with AML, namespace, resource, event and hardware support. It is larger and C-based, but remains the fallback if uACPI exposes a compatibility issue with this firmware. It is also useful as an offline validation/disassembly tool for the laptop's tables.

## Integration constraints

uACPI is C, but that is not a blocker: expose its host API as `extern "C"` functions implemented by Rust, and compile the pinned C sources as freestanding AArch64 objects.

The kernel API needs to map to existing or planned kernel services:

| uACPI host API | SketchOS implementation |
|---|---|
| RSDP lookup | physical RSDP address from versioned BootInfo |
| Physical map/unmap | existing linear physmap, bounds-checked to the supported physical range; unmap is a no-op while the map is permanent |
| MMIO | volatile 8/16/32/64-bit accesses through Device mappings |
| Allocation | kernel heap, only after heap initialization |
| Nanoseconds/stall/sleep | architectural timer; scheduler-backed sleep once scheduler is online |
| Thread ID | stable per-CPU/thread identity |
| Semaphores | kernel semaphore primitive; must support timeout and interrupt-safe signal |
| Interrupt registration | GIC IRQ registry |
| Deferred work | preallocated CPU0 work queue for GPE execution plus general notification work |
| PCI config | MCFG/ECAM-backed config access; writes remain disabled until PCI ownership is implemented |
| SystemIO | unsupported on this AArch64 target unless firmware actually describes a supported port-I/O mechanism |

Do not claim an operation-region address space is supported until its handler is installed and tested. In particular, GPIO and GenericSerialBus/I2C handlers depend on working Qualcomm GPIO/QUP drivers.

## Initialization order

1. Keep the current small static parser for boot-critical MADT/GTDT/IORT information until the AML path is proven.
2. Initialize physical memory management, the kernel heap, timer, GIC and the minimal PCI config reader.
3. Initialize uACPI's table subsystem from the BootInfo RSDP.
4. Register safe SystemMemory and PCI_Config operation-region handlers.
5. Initialize uACPI and load DSDT/SSDT tables.
6. Enumerate namespace objects and extract device IDs/resources. Start with read-only `_HID`, `_CID`, `_UID`, `_ADR` and `_CRS` evaluation.
7. Add GPIO, I2C/GenericSerialBus and power-resource handlers as the matching Qualcomm controllers become operational.
8. Only then enable full namespace initialization that invokes `_STA` and `_INI`; do not execute those methods against unimplemented operation regions.
9. Add GPE, SCI, Notify and wake handling after the GIC interrupt registry and deferred work queue are ready.

Do not enable the compile-time `UACPI_BAREBONES_MODE`: it strips AML and exposes only table access. Do not enable `UACPI_REDUCED_HARDWARE` merely because the FADT's hardware-reduced flag is set; that build option strips the event subsystem too. First inspect the actual FADT flags and determine which GPE/event functionality this laptop needs.

## Required firmware input

The repo currently has a static hardware inventory, but not the full raw AML tables. The earlier local export workspace was `C:\SketchOS\hardware\ACPI`; only `APIC.dat` was available in the prior review.

Before writing device-specific AML handlers, add the original binary tables under `hardware/acpi/`:

- `DSDT.dat`
- **every** `SSDT*.dat` (do not keep only the first SSDT)
- `FACP.dat` (FADT)
- `GTDT.dat`
- `SPCR.dat`
- `DBG2.dat`
- `IORT.dat`
- `MCFG.dat`
- `APIC.dat`
- `table-list.csv` with signature, length, revision, OEM ID and OEM table ID

Keep the binary tables unchanged. Disassembly can be generated separately with ACPICA `iasl`, so the original AML remains available for reproducible tests.

## Acceptance criteria

- Pinned uACPI 7.0.0 builds for `aarch64-unknown-none` with no hosted libc dependency.
- All kernel API calls have bounded, documented behavior; no placeholder success for unsupported hardware operations.
- DSDT and all SSDTs load without interpreter errors or kernel panic on the laptop's actual tables.
- Device resource extraction matches the AML and Windows/firmware inventory.
- No `_INI`, power method, GPIO or I2C method is executed before its operation-region handler is installed.
- The keyboard and touchpad are only marked supported after their I2C/GPIO resources, power sequencing and HID reports work on the laptop.
