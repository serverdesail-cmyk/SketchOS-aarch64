# Milestone 0.0.1 record

**Status:** complete and verified under QEMU. Superseded by
[milestone 0.0.2](milestone-0.0.2.md).

Milestone 0.0.1 established the boot path and the firmware inventory. It did
not take over the MMU, install exception vectors, deliver interrupts, schedule
threads, or bind drivers.

## What was verified

- A UEFI loader (`BOOTAA64.EFI`, target `aarch64-unknown-uefi`) loads a
  separate freestanding ELF64 AArch64 kernel from `\EFI\SketchOS\kernel.elf`.
- The kernel is `ET_EXEC`, linked at `0x40200000`, and requires
  `p_vaddr == p_paddr` for every `PT_LOAD` segment.
- A versioned 208-byte `BootInfo` record crosses the handoff. The loader and
  kernel both check its magic, version, size and layout.
- The loader obtains the final UEFI memory map, allocates the boot record and
  descriptor array as `EfiLoaderData`, and calls `ExitBootServices` correctly.
- The kernel never calls UEFI Boot Services or Runtime Services after handoff.
- ACPI comes only from the UEFI configuration table. The RSDP and XSDT
  checksums are validated. The RSDP is not assumed to be 16-byte aligned.
- The kernel decodes and reports:
  - firmware vendor and revision,
  - CPUs and MPIDR values from MADT,
  - GIC distributor, redistributor ranges and ITS,
  - GTDT timer interrupts and PSCI conduit,
  - MCFG PCIe ECAM windows,
  - SPCR and DBG2 debug ports,
  - SRAT/SLIT/PPTT topology data when present,
  - the final UEFI memory map,
  - the driver-API device vocabulary.
- The kernel prints `SKETCHOS 0.0.1 KERNEL OK` and, under QEMU, powers off with
  `QEMU: PSCI SYSTEM_OFF`.
- `tools/smoke-test.ps1` boots QEMU, checks the important log lines, requires
  the success marker and power-off, and fails on timeout.

## QEMU environment

```text
-machine virt,gic-version=3 -cpu max -smp 4 -m 2048
```

Observed inventory: 8 ACPI tables, 4 CPUs, GICv3 distributor at
`0x08000000`, redistributor at `0x080a0000`, ITS at `0x08080000`, GTDT timer
GSIVs 29/30/27/26, MCFG segment 0 ECAM at `0x4010000000`, PL011 SPCR at
`0x09000000`, 36 memory descriptors, 2042 MiB usable.

## What 0.0.1 did not do

- It did not own the MMU or build page tables.
- It did not install exception vectors or deliver interrupts.
- It did not program the GIC or the generic timer.
- It did not schedule anything.
- It did not bind or start a driver.
- It had no heap, userspace, filesystem, storage, GPU, network or input support.
- It made no performance claim.

## Evidence

The milestone was validated with:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -TimeoutSeconds 180
```

The log ended with the success marker and PSCI power-off, and the script
reported `PASS`.
