# Milestone 0.0.2 record

**Status:** complete and verified under QEMU on 2026-11-09.

This record covers what the 0.0.2 kernel actually does, how it was verified, and
what it deliberately does not do. It supersedes the 0.0.1 record but does not
claim any capability that was not observed in the QEMU serial log.

## Scope

Milestone 0.0.2 turns the 0.0.1 boot-and-inventory kernel into a kernel that
owns its address space, handles exceptions, enumerates PCIe, brings up the
interrupt controller and timer, and runs preemptive kernel threads.

Added in this milestone:

1. Kernel-owned AArch64 page tables and MMU enablement.
2. Fatal exception vectors with a complete general-purpose and FP/SIMD trap
   frame.
3. PCIe ECAM enumeration driven by MCFG and bridge secondary-bus links.
4. GICv3/GICv4 distributor, redistributor and system-register CPU-interface
   bring-up.
5. The non-secure EL1 physical timer at 100 Hz.
6. A preemptive round-robin scheduler with four kernel threads.
7. A framebuffer text console, exercised through QEMU `ramfb`.
8. Version strings and documentation updated from 0.0.1 to 0.0.2.

## MMU

The kernel builds its own identity map before any device is touched:

- 4 KiB translation granule, 48-bit virtual address space (`TCR_EL1.T0SZ = 16`).
- 2 MiB blocks for bulk mappings and 4 KiB pages at region edges.
- `MAIR_EL1` attributes:
  - Attr0: Device-nGnRnE for MMIO.
  - Attr1: Normal write-back, inner-shareable, for RAM.
  - Attr2: Normal non-cacheable for the framebuffer.
- RAM from the final UEFI memory map is Normal write-back.
- Every ACPI-described MMIO window is explicitly Device.
- The framebuffer is Normal non-cacheable so pixel writes reach the display
  without cache maintenance.
- Mappings are non-executable unless they are kernel text.

Observed log:

```text
Exceptions: VBAR_EL1 0x0000000040200800, table aligned: yes
MMU: enabled, 4K granule, identity map, TTBR0 0x0000000040224000, 4 L2 / 13 L3 tables
```

The normal QEMU run used 13 L3 tables; the framebuffer run used 14 because the
GOP framebuffer added a page-granular region.

## Exception vectors

`vectors.S` installs a 2 KiB-aligned table and builds an 816-byte trap frame for
every entry:

| Offset | Contents |
| --- | --- |
| 0 | `x0..x30` |
| 248 | `ELR_ELx` |
| 256 | `SPSR_ELx` |
| 264 | `ESR_ELx` |
| 272 | `FAR_ELx` |
| 280 | reserved |
| 288 | `q0..q31` |
| 800 | `FPSR` |
| 808 | `FPCR` |

Synchronous exceptions, FIQs and SErrors are fatal and print the vector, ELR,
SPSR, ESR, FAR and the general-purpose registers. IRQs return through the same
restore path used to start the first task. `CPACR_EL1.FPEN` is set to `0b11` in
`boot.S` before any vector can run.

## PCIe

Enumeration starts at each MCFG window's first bus and follows only the
secondary-bus numbers reported by PCI-to-PCI bridges. It never probes an
undescribed bus. BARs are read for reporting but are not sized or assigned.

Observed log for QEMU `virt`:

```text
PCIe ECAM windows:
  segment 0: bus 00-ff (256 buses), ECAM 0x0000004010000000, 262144 KiB
  pci:0000:00:00.0 1b36:0008 bridge host-bridge
  pci:0000:00:01.0 1af4:1001 storage device bar 0x0000000000000000 io
PCIe: 2 functions on 1 buses across 1 segments
```

## Interrupt controller and timer

The kernel uses the GICv3/GICv4 system-register CPU interface (`ICC_*_EL1`),
not a guessed GICv2 memory layout. It:

- Finds the redistributor whose `GICR_TYPER` affinity matches the boot CPU,
  normalizing MPIDR Aff3 from bits 39:32 to the packed 32-bit affinity value.
- Detects the 256 KiB GICv4 redistributor stride from `GICR_TYPER.VLPIS`.
- Wakes the redistributor and waits for `ChildrenAsleep` to clear.
- Enables the timer PPI as Group 1 non-secure, level-sensitive, normal priority.
- Enables `ICC_SRE_EL1`, sets `ICC_PMR_EL1`, `ICC_BPR1_EL1` and
  `ICC_IGRPEN1_EL1`.

The distributor is configured through the **non-secure view** of `GICD_CTLR`,
where `ARE_NS` is bit 4 and `EnableGrp1A` is bit 1. `GICD_TYPER` has no
affinity-routing capability bit; GICv3 mandates affinity routing, so the
distributor version is the only check.

Observed log:

```text
GIC: GICv3+ distributor 0x0000000008000000, redistributor 0x00000000080a0000, timer PPI 30
Timer: CNTP 100 Hz, period 10000000 ticks at 1000000000 Hz
```

The timer PPI comes from GTDT (`EL1 non-secure GSIV 30`). The default is the
architectural non-secure EL1 physical timer PPI, 30.

## Scheduler

- Four kernel threads on the boot CPU.
- 16 KiB stack per thread.
- Preemptive round-robin with a 5-tick (50 ms) timeslice.
- The demo runs for 200 ticks (two seconds), then reports and powers off.
- Each task performs integer and floating-point work, so the switch exercises
  `q0..q31`, `FPSR` and `FPCR`.

Observed log:

```text
Scheduler: 4 tasks, 16 KiB stacks, round-robin, 5-tick timeslice, 200-tick demo

Scheduler: demo complete, 200 ticks, 39 context switches
  task 0: 59146290 iterations, fp bits 0x41cb86116b4dc65d
  task 1: 58448414 iterations, fp bits 0x41c9a9d2a3418553
  task 2: 58513020 iterations, fp bits 0x41c9d4880f37fe15
  task 3: 58421953 iterations, fp bits 0x41c9986a8e0d4e3d
SKETCHOS 0.0.2 KERNEL OK
QEMU: PSCI SYSTEM_OFF
```

The exact iteration counts and FP bit patterns vary between runs; the tick
count, the switch count and the success marker are asserted by the smoke test.

## Framebuffer console

The loader already reported a GOP framebuffer when one exists. The kernel now
uses it for an 8x8 text console at scale 2, in addition to the PL011 serial
console. QEMU's `-display none` configuration has no GOP framebuffer, so the
normal smoke test does not exercise it. The `-Framebuffer` smoke test adds
`-device ramfb` and observes:

```text
GOP framebuffer: 800x600 stride 800 bgr @ 0x00000000bc7a0000 (3 MiB)
Framebuffer console: active
```

## Verification

Both commands were run from the repository root and passed:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -TimeoutSeconds 180
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -Framebuffer -TimeoutSeconds 180
```

The smoke test asserts:

- the loader and kernel banners,
- `Exceptions: VBAR_EL1 ... table aligned: yes`,
- `MMU: enabled, 4K granule, identity map`,
- the ACPI inventory and PCIe enumeration,
- `GIC: GICv3+ ...`,
- `Timer: CNTP 100 Hz`,
- `Scheduler: 4 tasks ...`,
- `Scheduler: demo complete, 200 ticks, N context switches` with `N > 0`,
- `SKETCHOS 0.0.2 KERNEL OK`,
- `QEMU: PSCI SYSTEM_OFF`.

In the framebuffer run it additionally asserts `Framebuffer console: active`.

## Known limitations

- Only the boot CPU is initialized. MADT reports four QEMU CPUs and eight
  enabled laptop CPUs, but no secondary CPU is started.
- The MMU and GIC CPU interface require EL1. No EL2-to-EL1 drop is implemented.
- Synchronous exceptions are fatal. There is no page-fault recovery, no
  copy-on-write, and no demand paging.
- There is no kernel heap. All kernel data structures are static.
- No driver is bound. PCIe functions are enumerated and reported only.
- No storage, filesystem, userspace, system calls, GPU, network, USB, input,
  audio, camera, or panel driver exists.
- No performance claim is made. The reported inventory time is a diagnostic,
  not a benchmark; the framebuffer run is slower because console writes are
  synchronous MMIO.
- The physical laptop has not been booted. Its SPCR UART is interface `0x13`,
  not a PL011, so the current kernel cannot print to it.
- Internal-disk installation is explicitly out of scope until the USB path is
  verified on the machine with user confirmation.
