# Hardware compatibility matrix (historical)

> **Historical note:** this was the 0.0.2-era matrix. The maintained matrix is
> [hardware-compatibility.md](hardware-compatibility.md). Keep this file for
> context, but update the maintained matrix instead.

This document separates three very different kinds of statement:

1. **QEMU-verified** — observed in the QEMU serial log and asserted by
   `tools/smoke-test.ps1`.
2. **Laptop firmware fact** — read from the ACPI dumps in
   `C:\SketchOS\hardware` (not modified by this repository).
3. **Unimplemented** — described by firmware but not driven by SketchOS.

Nothing in category 3 should be read as working.

## Development target

| Property | Value |
| --- | --- |
| Machine | HP OmniBook X 14-fe1xxx |
| SoC | Snapdragon X Plus X1P42100 |
| CPUs | 8 enabled Oryon cores out of 12 MADT GICC entries |
| RAM | 16 GiB |
| GPU | Adreno X1-45 (`QCOM0D17`) |
| Display | 2240x1400 panel |
| Firmware | BIOS F.24 |
| ACPI source | `C:\SketchOS\hardware\ACPI-summary.txt` and the raw tables |

## Boot and core kernel

| Component | QEMU | Laptop | State |
| --- | --- | --- | --- |
| UEFI loader | EDK2 AArch64, verified | Not booted | QEMU-verified |
| ELF64 kernel handoff | Verified | Not booted | QEMU-verified |
| `ExitBootServices` handoff | Verified | Not booted | QEMU-verified |
| ACPI RSDP/XSDT validation | Verified | Tables present | QEMU-verified |
| Kernel-owned MMU | Verified | Not booted | QEMU-verified |
| Exception vectors | Installed | Not booted | QEMU-verified |
| PCIe ECAM enumeration | Verified (2 functions) | 8 MCFG segments described | QEMU-verified |
| GICv3/GICv4 bring-up | Verified (GICv3) | GICD v4 at `0x17000000` | QEMU-verified; laptop untested |
| Generic timer | Verified, 100 Hz | GTDT system-register timer | QEMU-verified; laptop untested |
| Preemptive scheduler | Verified, 4 tasks | Not booted | QEMU-verified |
| SMP startup | Not implemented | 8 cores described | Unimplemented |
| Kernel heap | Not implemented | — | Unimplemented |
| Userspace | Not implemented | — | Unimplemented |
| Filesystem | Not implemented | — | Unimplemented |

## Laptop interrupt and timer description

| Object | Firmware value |
| --- | --- |
| GIC distributor | `0x17000000`, GICv4 |
| GIC ITS | `0x17040000` |
| GIC redistributors | `0x17080000` .. `0x17340000`, step `0x40000` |
| Performance-monitor GSIV | 23 |
| Efficiency classes | 0 and 1 |
| FADT ARM boot flags | `0x0001` (PSCI, SMC conduit) |
| DSDT | `0xd4782018` |
| GTDT timer | System-register timer |
| SPCR | Base `0x894000`, interface `0x13`, GSIV 386, baud code 7 |
| DBG2 | 7 entries, including `\_SB.UARD` and `\_SB.URS0/1/2` |
| PPTT | 20 nodes, 3 caches |
| IORT | 30 nodes |
| TPM2 | Control area `0x81f10000`, start method 14 |
| BGRT | Image `0xcfdaa018` |

The GICv4 redistributor stride (`0x40000`) matches the driver's
`GICR_TYPER.VLPIS` detection path, but that path has only been exercised on
QEMU's GICv3 model so far.

## Laptop PCIe ECAM windows

| Segment | Base | Buses |
| --- | --- | --- |
| 0 | `0x400000000` | 00-ff |
| 1 | `0x500000000` | 00-ff |
| 2 | `0x6000000000` | 00-ff |
| 3 | `0x740000000` | 00-01 |
| 4 | `0x7c000000` | 00-01 |
| 5 | `0x7e000000` | 00-01 |
| 6 | `0x70000000` | 00-01 |
| 7 | `0x74000000` | 00-01 |

The current enumerator can walk these windows after the MMU maps them. It has
not been run on the laptop.

## Laptop devices described by firmware

The hardware dump contains 344 records, 141 ACPI devices and 107 distinct
hardware IDs. Important examples:

| Device | Identifier | SketchOS state |
| --- | --- | --- |
| GPU | `QCOM0D17` | Unimplemented |
| NVMe controller | PCI `144D:A80D` | Unimplemented |
| Wi-Fi | PCI `17CB:1103` | Unimplemented |
| Bluetooth | `QCOM0C6B` | Unimplemented |
| Cameras | `QCOM0C32`, `VEN_QCOM&DEV_0C25` | Unimplemented |
| Audio DSP | `QCOM0C1B` | Unimplemented |
| USB controllers | `QCOM0D08`, `QCOM0C8B`, `QCOM0C8C` | Unimplemented |
| Touchpad | `ELAN010E` | Unimplemented |
| Keyboard | `QTEC0001` | Unimplemented |
| PCIe root complexes | `PNP0A08` (two) | Enumerated only |
| IOMMUs | `QCOM068F` (two) | Unimplemented |

## Serial console

QEMU's SPCR describes a PL011 at `0x09000000`, so the kernel drives it and the
smoke test can capture a log.

The laptop's SPCR describes interface type `0x13` at `0x894000`, which is a
Qualcomm/SDM845-style UART, **not** a PL011. The kernel intentionally refuses to
guess its register layout. Until a documented driver exists, the laptop produces
no SketchOS serial output. This is a hard limitation, not a bug to paper over.

## Display

QEMU with `-display none` exposes no GOP framebuffer. The `-Framebuffer` smoke
test adds `-device ramfb` and verifies the framebuffer text console. The
laptop's panel is a different device and is not driven by the current
framebuffer code, which only consumes the GOP framebuffer the loader reports.

## What is verified today

Only the QEMU column. The laptop column records firmware facts and a bring-up
plan; it is not a claim that SketchOS runs on the machine. See
[real-hardware.md](real-hardware.md) for the gated bring-up procedure.
