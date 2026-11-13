# SketchOS hardware compatibility matrix

Maintained document. Last audit: 2026-11-12 (milestone 0.0.6, QEMU only).

This file records what firmware describes, what SketchOS actually drives, and what is
still missing. A hardware identifier in this document is **not** a support claim.
Only a row whose driver status says **Verified** on a platform has been tested on
that platform.

## Status legend

| Status | Meaning |
| --- | --- |
| **Verified** | Driver initialized the device and the result was observed in a test log. |
| **Decoded only** | Firmware table or identifier is parsed and reported; the device is not driven. |
| **Enumerated only** | Bus enumeration found the device; no driver is bound. |
| **Blocked** | A specific missing capability prevents progress (for example, an unknown UART register layout). |
| **Unimplemented** | No driver exists. |
| **Not present** | The platform does not describe the device. |

## Platform profiles

| Profile | Identity | Kernel state | Test state |
| --- | --- | --- | --- |
| QEMU virt | EDK2 AArch64, ACPI OEM `BOCHS `, GICv3, PL011 | Boots to the 0.0.6 success marker; higher-half kernel with linear physmap, W^X audit and recoverable faults verified; eight CPUs online; PPTT cluster decode, EEVDF per-core queues, cluster-aware stealing and one-shot timers verified; allocators and IPIs verified; EL0 userspace with per-process address spaces, a static ELF loader, seven system calls, bounded IPC, `sleep`, a deliberate user fault, a two-thread process and a wake-latency workload verified; EL2 entry, relocatable load and standalone ESP-image boot verified | **Verified** by `tools/smoke-test.ps1` (normal, two-cluster, framebuffer, relocation, EL2, debug and image modes) |
| HP OmniBook X 14-fe1xxx | Snapdragon X Plus X1P42100, BIOS F.24 | Not booted | Firmware facts only; no physical test |
| Generic ARM64 | Any ACPI/UEFI ARM64 platform | Not validated | Untested |

The laptop has 8 enabled Oryon CPUs out of 12 MADT GICC entries, 16 GiB RAM, an
Adreno X1-45 GPU, and a 2240x1400 internal panel. These are firmware facts, not
driver results.

## Verified baseline: QEMU virt

| Component | QEMU value | Status | Evidence |
| --- | --- | --- | --- |
| UEFI loader | EDK2 AArch64 | Verified | Loader banner and handoff |
| ELF loading | Relocatable `ET_DYN`, loader-chosen base below 4 GiB | Verified | `Kernel image: ET_DYN, 60 relocations`; `Kernel entry: physical 0x00000000bba29000, virtual 0xffff8000bba29000` |
| Boot ABI | Version 2, 272-byte record, physical pointers | Verified | `Boot info: ..., version 2, 272 bytes` |
| Higher half | `0xffff8000...` kernel image plus `0xffffc000...` linear physmap | Verified | `MMU: higher half, TTBR1 ... physmap 3 GiB`; `running at 0xffff8000..., inside the higher-half image: yes` |
| W^X | Per-section permissions, no writable-executable leaf | Verified | `MMU: W^X audit clean (no writable-executable leaf)` |
| Recoverable faults | Probe read/write/execute resume at a saved `ELR`; EL0 faults terminate the process | Verified | `MMU: fault self-test passed (unmapped read, read-only write, NX execute)`; `pid 4 fault (ESR 0x0000000092000006, FAR 0x0000000000001000)` |
| Address spaces | Per-space TTBR0 root and ASID; low half sealed after SMP and unsealed on the boot core for the user phase | Verified | `MMU: address-space self-test passed`; `MMU: TTBR0 walks disabled; low half sealed`; `Userspace: 6 processes` |
| User ELF loader | Static non-PIE `ET_EXEC` AArch64 images, page permissions, BSS, initial stack | Verified | `Userspace: 6 processes, 8 threads, 46 pages, 16 KiB user stacks` |
| System-call ABI | Version `0x0001_0000`; calls 0-6 (`abi_version`, `exit`, `write`, `yield`, `send`, `recv`, `sleep`) | Verified | `init: syscall ABI 0x00010000`; every user line in the log; latency workload sleep/wake lines |
| EL0 execution | Per-process TTBR0, W^X user pages, kernel stacks with guard pages | Verified | `init: hello from EL0`; six pids reaped with `0 user pages` |
| IPC | Synchronous, bounded 64-byte point-to-point messages | Verified | `ipc: receiver got ping`; `ipc: sender got pong` |
| User fault routing | Unrecoverable EL0 fault reaps only the faulting process | Verified | `Userspace: pid 4 fault (ESR ...), terminating process`; `pid 4: terminated by fault, status 139` |
| EL2 entry | `virtualization=on`; kernel drops to EL1h | Verified | `Entry exception level: EL2, running at EL1` |
| Load-address independence | `-OccupyLegacy` reserves `0x40200000` | Verified | `Relocation test: occupied legacy kernel address 0x0000000040200000`; kernel boots at another base |
| ACPI RSDP/XSDT | QEMU/EDK2 tables | Verified | Checksum-validated walk |
| Memory map | UEFI map, 36-37 descriptors in test runs | Verified | Kernel inventory |
| MMU | 4 KiB granule, 48-bit VA, four levels, split TTBR0/TTBR1 | Verified | `MMU: higher half, TTBR1 ..., 24 tables of 2048, physmap 3 GiB`; `TCR 0x00000005b5103510, MAIR 0x00000000004400ff` |
| Physical allocator | UEFI map, 4 KiB pages, 32 GiB bitmap span | Verified | `Physical self-test: PASS (... scrub, reservations)` |
| Kernel heap | Address-ordered first fit, 8 MiB cap | Verified | `Heap self-test: PASS (... growth, growth cap, ...)` |
| GIC | GICv3, distributor `0x08000000`, per-core redistributors | Verified | `GIC: GICv3+ ... timer PPI 30`; each core reports its `GICR` base |
| Generic timer | CNTP, one-shot, 1 GHz counter | Verified | `Timer: CNTP one-shot, 1000000000 Hz counter, 1 ns resolution`; every core arms its own events |
| SMP discovery | MADT enabled `GICC` entries | Verified | `SMP: 8 discovered, 8 expected online` |
| Secondary startup | PSCI `CPU_ON`, 8 CPUs | Verified | `core 1..7: online ... startup ... us, GICR ...` |
| IPIs | GICv3 SGIs 0-3 via `ICC_SGI1R_EL1` | Verified | `IPI: 7 secondaries answered a ping/pong round trip` |
| Topology | ACPI PPTT parent links; MPIDR fallback | Verified | `PPTT: 10 nodes / 0 caches / 1 clusters`; two-cluster mode reports `12 nodes / ... / 2 clusters` |
| Scheduler | Per-core EEVDF AVL queues, cluster-aware stealing, one-shot timer, tickless idle | Verified | `8 per-core run queues`; `7 steals, 7 migrations`; `15 idle disarms`; `SMP: concurrency verified` |
| PCIe | ECAM `0x4010000000`, 2 functions | Enumerated only | `PCIe: 2 functions on 1 buses` |
| PL011 | SPCR PL011 at `0x09000000` | Verified | Serial log captured |
| Framebuffer | ramfb GOP, 1024x768 BGR | Verified with `-Framebuffer` | `Framebuffer console: active` |
| virtio block | PCI `1af4:1001` | Enumerated only | PCIe inventory |
| PSCI | FADT-compliant, HVC in normal/relocation runs, SMC under `-Virtualization` | Verified | `SMP: 8 discovered ... PSCI compliant (HVC)`; `QEMU: requesting PSCI SYSTEM_OFF` |

## Laptop platform facts

These values come from the ACPI/PCI research dump in `C:\SketchOS\hardware` and are
reproduced here so the matrix is self-contained. They have not been re-read from the
laptop by SketchOS.

| Object | Firmware value |
| --- | --- |
| GIC distributor | `0x17000000`, GICv4 |
| GIC ITS | `0x17040000` |
| GIC redistributors | `0x17080000` .. `0x17340000`, step `0x40000` |
| Performance-monitor GSIV | 23 |
| Efficiency classes | 0 and 1 |
| FADT ARM boot flags | `0x0001` (PSCI compliant, SMC conduit) |
| GTDT | System-register timer |
| SPCR | Base `0x894000`, interface `0x13`, GSIV 386, baud code 7 |
| DBG2 | 7 entries, including `\_SB.UARD` and `\_SB.URS0/1/2` |
| PPTT | 20 nodes, 3 caches |
| IORT | 30 nodes |
| TPM2 | Control area `0x81f10000`, start method 14 |
| BGRT | Image `0xcfdaa018` |
| DSDT | `0xd4782018` |

### Laptop PCIe ECAM windows

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

The current enumerator can walk these windows only after the MMU maps them. It has
not been run on the laptop.

## Core platform devices

| Device | Hardware identifier | Supported platform | Required firmware | Driver status | Initialization requirements | Interrupt requirements | DMA / IOMMU | Power management | Test results | Known limitations |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| GIC distributor | ACPI MADT GICD; laptop `0x17000000` v4, QEMU `0x08000000` v3 | QEMU virt verified; laptop untested | MADT with GICD/GICR entries; GICv3/v4 system-register interface | **Verified on QEMU**; laptop untested | Map distributor Device-nGnRnE; read GICD_TYPER/DS; enable non-secure Group 1; wait RWP | Timer PPI 30 on QEMU; laptop PPI/SPI assignments not yet driven | None | None | Timer, IPIs and scheduler run on all eight QEMU cores | Assumes one security-state view in `GICD_CTLR`; no GICv2; no SPI routing |
| GIC redistributors | ACPI MADT GICR; laptop `0x17080000`..`0x17340000`, step `0x40000` | QEMU verified on all eight cores; laptop untested | MADT GICR ranges; GICv4 VLPI frame if present | **Verified on QEMU** (every online core) | Map each range; detect 256 KiB vs 128 KiB stride from GICR_TYPER.VLPIS; wake redistributor; program PPI and SGIs | Timer PPI and SGIs 0-3 | None | None | Each core finds its frame by MPIDR, wakes it and initializes its own CPU interface | `GICR_TYPER == 0` frames are skipped; no ITS/LPI; no SPI routing |
| GIC ITS | ACPI MADT ITS; laptop `0x17040000` | None | MADT ITS entry; GICv4 | **Decoded only** | Map ITS; allocate device tables/collections; enable LPIs | MSI/MSI-X to LPI | Requires IOMMU/SMMUv3 stream table | None | Not tested | No ITS or LPI support |
| Generic timer | ACPI GTDT; laptop system-register timer | QEMU verified on every core; laptop untested | GTDT non-secure EL1 timer GSIV; `CNTFRQ_EL0` | **Verified on QEMU** | Program `CNTP_CTL_EL0`/`CNTP_CVAL_EL0`; read `CNTFRQ_EL0`; each core programs its own timer | Per-core PPI 30 on QEMU | None | None | One-shot events on every online core; 1000-ms EEVDF demo, 15 tickless idle disarms | Uses `CNTVCT_EL0` for time but `CNTP_*` for interrupts; no wall clock or timekeeping |
| PSCI | ACPI FADT ARM boot flags; laptop `0x0001` (SMC) | QEMU verified; laptop not invoked | FADT PSCI-compliant flag and conduit; `PSCI_VERSION` | **Verified on QEMU** (`CPU_ON` and `SYSTEM_OFF`) | Use SMC or HVC per FADT; 64-bit `CPU_ON` only for PSCI 0.2+; `SYSTEM_OFF` always uses the 32-bit ID | None | None | `CPU_ON`, `SYSTEM_OFF` | Eight CPUs started and the machine powered off | No `CPU_OFF`, `SUSPEND`, hotplug or affinity-info use; QEMU flag gates power-off |
| SPCR UART | ACPI SPCR; laptop interface `0x13` at `0x894000`; QEMU interface `0x03` at `0x09000000` | QEMU PL011 verified; laptop **Blocked** | SPCR GAS; for PL011, correct reference clock | **Verified on QEMU**; laptop **Blocked** | PL011: clock divisor, FIFO, CR; laptop: unknown Qualcomm register layout | Laptop GSIV 386; not used | None | None | QEMU serial log; laptop silent | Hard-coded 24 MHz PL011 clock; no Qualcomm UART driver; early laptop output is lost |
| DBG2 debug ports | ACPI DBG2; laptop 7 entries including `\_SB.UARD` | None | DBG2 | **Decoded only** | Namespace and GAS parsing | GSIV from DBG2/SPCR | None | None | Not tested | No debug-port driver |
| PCIe root complexes | ACPI `PNP0A08` (two on laptop) | None | MCFG ECAM windows; ACPI namespace | **Enumerated only** | Map ECAM; walk buses; assign BARs | MSI/MSI-X/INTx | Requires IOMMU for safe DMA | PCIe power/clock control | QEMU ECAM walk; laptop not run | No BAR assignment, no config writes, no hotplug, no power management |
| TPM 2.0 | ACPI TPM2; laptop control area `0x81f10000`, start method 14 | None | TPM2 table; CRB or FIFO interface | **Decoded only** | Map control area; follow start method; command interface | None | None | None | Not tested | No TPM command driver |
| BGRT | ACPI BGRT; laptop image `0xcfdaa018` | None | BGRT | **Decoded only** | Parse image header; map image | None | None | None | Not displayed | No boot-logo rendering |

## Laptop device matrix

| Device | Hardware identifier | Supported platform | Required firmware | Driver status | Initialization requirements | Interrupt requirements | DMA / IOMMU | Power management | Test results | Known limitations |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Adreno X1-45 GPU | ACPI `QCOM0D17`; Adreno X1-45 | None | GPU firmware/microcode, board data, power-domain and clock descriptions | **Unimplemented** | Map GPU registers; load firmware; initialize IOMMU; set clocks/regulators; allocate command buffers | GPU interrupts; likely MSI or wired SPI; exact routing not yet decoded | Requires SMMUv3/IOMMU and GPU VA space | GPU power domains, clocks, thermal limits | Not tested | No GPU driver, no firmware loader, no display pipeline, no IOMMU |
| Samsung NVMe controller | PCI `144D:A80D` | None | PCIe ECAM, BAR assignment, controller firmware in device | **Unimplemented** | Map BAR0; reset controller; create admin/I/O queue pairs; identify namespace; enable bus mastering | MSI-X or INTx; MSI-X requires ITS/LPI or GIC SPI | Requires DMA-safe memory and IOMMU stream mapping | NVMe power states, PCIe ASPM | Not tested | No block layer, no PCIe driver, no MSI-X, no DMA/IOMMU |
| Qualcomm Wi-Fi | PCI `17CB:1103` | None | Board firmware blob, NVM/calibration data, PCIe BARs | **Unimplemented** | Map BARs; load firmware; configure DMA rings; initialize MAC/PHY | MSI/MSI-X or SPI; routing not decoded | Requires DMA and IOMMU | Power domains, clocks, RF enable, thermal | Not tested | No firmware loader, no network stack, no DMA/IOMMU |
| Qualcomm Bluetooth | ACPI `QCOM0C6B` | None | Transport firmware (UART or PCIe), board data | **Unimplemented** | Determine transport; load firmware; configure flow control | Transport IRQ/GPIO; not decoded | DMA if PCIe transport | Power enable, clocks | Not tested | No transport driver, no firmware loader |
| ELAN touchpad | ACPI `ELAN010E` | None | ACPI namespace/AML, I2C HID descriptor, GPIO/EC methods | **Unimplemented** | Resolve ACPI resources; initialize I2C controller; HID reset; read report descriptor | GPIO interrupt; exact GSIV/GPIO not decoded | None (I2C) | Sleep/wake, power GPIO | Not tested | No ACPI namespace/AML, no I2C driver, no HID stack |
| QTEC keyboard | ACPI `QTEC0001` | None | ACPI namespace/AML, keyboard controller description | **Unimplemented** | Resolve ACPI resources; initialize controller; translate scancodes | Wired interrupt; not decoded | None | Sleep/wake | Not tested | No input stack, no ACPI namespace/AML |
| Qualcomm audio DSP | ACPI `QCOM0C1B` | None | Audio DSP firmware, topology, board data | **Unimplemented** | Load DSP firmware; configure clocks/regulators; set up PCM/compress paths | DSP interrupts; not decoded | Requires DMA and IOMMU | Audio power domains, clocks, jack detection | Not tested | No audio driver, no firmware loader, no DMA/IOMMU |
| Qualcomm USB controllers | ACPI `QCOM0D08`, `QCOM0C8B`, `QCOM0C8C` | None | PCIe/ACPI resources, controller firmware if required | **Unimplemented** | Map registers; reset controller; set up xHCI rings; enumerate ports | xHCI interrupt; MSI/MSI-X or SPI; not decoded | Requires DMA and IOMMU | USB power domains, clocks, PHY, runtime PM | Not tested | No XHCI driver, no USB stack, no DMA/IOMMU |
| Internal display panel | 2240x1400; panel described by ACPI/DSI | None | Panel init sequence, DSI controller, backlight, GPU display pipeline | **Unimplemented** | Initialize DSI/PHY; send panel init; configure timing; enable backlight | Panel/DSI interrupts; not decoded | GPU/display DMA and IOMMU | Panel power, backlight, GPU power domains | Only GOP framebuffer text on QEMU; laptop panel not driven | No DSI/panel driver; GOP framebuffer is not the native panel path |
| Battery and thermal sensors | ACPI namespace (EC/thermal zones) | None | ACPI AML/EC, thermal zone definitions | **Unimplemented** | Evaluate AML; access EC; read battery/thermal registers | EC/thermal interrupts; not decoded | None | Thermal throttling, battery charging | Not tested | No AML interpreter, no EC driver, no thermal framework |
| Qualcomm IOMMU | ACPI `QCOM068F` (two); IORT 30 nodes | None | IORT/SMMUv3 stream table, device IDs, firmware configuration | **Unimplemented** | Discover SMMUv3; configure stream table; map device IDs; enable translation | SMMU interrupts; not decoded | This is the DMA/IOMMU foundation itself | SMMU power/clock | IORT nodes counted only | No SMMUv3 driver, no DMA ownership model |
| Cameras | ACPI `QCOM0C32`, `VEN_QCOM&DEV_0C25` | None | Camera firmware, sensor drivers, ISP configuration | **Unimplemented** | Not investigated | Not decoded | Requires IOMMU and DMA | Camera power domains | Not tested | Out of scope for this milestone |
| PCIe root complexes | ACPI `PNP0A08` (two) | None | MCFG; ACPI namespace | **Enumerated only** | ECAM mapping and bus walk | MSI/INTx | IOMMU for downstream devices | PCIe power/clock | QEMU walk only | No resource assignment or hotplug |

## Serial console: the laptop blocker in detail

QEMU's SPCR describes interface type `0x03` (PL011) at `0x09000000`, so the kernel
drives it and the smoke test captures a log.

The laptop's SPCR describes interface type `0x13` at `0x894000`. That is a
Qualcomm/SDM845-style UART, not a PL011. The kernel intentionally refuses to guess its
register layout (`kernel/src/console.rs:1-7`). Until a documented driver exists, the
laptop produces no SketchOS serial output. The GOP framebuffer console is the only
visible output path, and it starts only after the MMU step.

## Display and framebuffer

QEMU with `-display none` exposes no GOP framebuffer. The `-Framebuffer` smoke test
adds `-device ramfb` and verifies the text console. The laptop's 2240x1400 panel is a
different device: the current framebuffer code consumes only the GOP linear
framebuffer reported by the loader. It does not drive the panel, DSI, backlight, or
GPU.

## DMA and IOMMU requirements

No device driver can be considered safe until the following exist:

1. DMA-safe allocation with physical/virtual separation.
2. Cache maintenance and coherency rules for non-coherent devices.
3. SMMUv3 discovery from IORT.
4. Stream-table and device-ID mapping.
5. DMA map/unmap APIs with ownership and lifetime tracking.
6. Interrupt integration for MSI/MSI-X and wired SPIs.
7. Firmware loading and device reset/power sequencing.

The laptop's two `QCOM068F` IOMMUs and 30 IORT nodes are currently only counted
(`kernel/src/acpi/iort.rs`). No translation is programmed.

## Power-management requirements

The laptop devices need power domains, clocks, regulators, resets, and thermal limits
that are not modeled by the kernel. The current kernel has no clock, regulator,
reset, or runtime-PM framework. The 0.0.6 kernel can disarm its timer and wait in
`WFI` when a core has no work, but the demo workers still busy-spin while they
run and there is no DVFS or thermal control; this is not sufficient for battery
or thermal behavior.

## QEMU acceleration on this host (2026-11-12)

This section records two separate QEMU accelerators on the current host, not a
laptop result. Host: Windows `10.0.26300`, Arm64, Snapdragon X Plus X1P42100,
8 logical CPUs, `HypervisorPresent = True`, QEMU 11.1.0 with the `whpx` and
`tcg` accelerators. The installed QEMU is 11.1.0; the 0.0.5/0.0.6 test logs keep
their original 10.1.94 (v10.2.0-rc4) label.

WHPX is **not usable for SketchOS on this host**, for two independent reasons:

1. **The bundled EDK2 firmware does not boot.** With `-accel whpx` the serial log
   stops after the UEFI banner, `ArmTrngLib could not be correctly initialized`
   and three `Error: Image at ... start failed` lines; `BdsDxe` is never reached,
   so the loader never runs. This was reproduced with 1, 4 and 8 vCPUs;
   `-cpu host`, `max`, `cortex-a53` and `cortex-a57`; `gic-version=3`, `max` and
   the default; `highmem=off`; `its=off`; fresh and reused varstores; pflash and
   `-bios`; all hung identically. WHPX also rejects `kernel-irqchip=off` and
   `split` ("on Arm, only kernel-irqchip=on is currently supported"), rejects
   GICv4 ("WHPX does not support GICv4 emulation"), rejects guest virtualization
   extensions, and `gic-version=host` requires KVM.
2. **WHPX does not expose the EL1 physical timer.** A minimal bare-metal AArch64
   probe shows the GICv3 distributor, redistributor, system-register CPU
   interface and exception vectors all working under WHPX, and
   `mrs CNTPCT_EL0` returning a value. The next instruction, writing
   `CNTP_CVAL_EL0`, faults to the default exception handler. The same probe
   reworked to use the virtual timer (`CNTV_CVAL_EL0`, `CNTV_CTL_EL0`, PPI 27)
   prints `IRQ-OK` under WHPX. The 0.0.6 kernel programs `CNTP_CVAL_EL0` and
   `CNTP_CTL_EL0` directly (`kernel/src/arch/aarch64/timer.rs`), so even with
   working firmware it would fault during timer initialisation under WHPX.

TCG remains the only accelerator that boots the 0.0.6 kernel here. The smoke
test therefore keeps `-accel tcg,thread=multi,tb-size=256` as its default and
accepts `-Accel whpx` only as a diagnostic that is expected to fail. No kernel
timer fallback was added: selecting a different architectural timer is a kernel
design change, not a QEMU option, and it would still leave the firmware hang.

After this work the kernel **was** switched to the EL1 virtual timer (`CNTV_*`,
PPI 27) in `kernel/src/arch/aarch64/timer.rs` and `gic.rs`, because that is the
timer a hypervisor exposes (WHPX's vGIC wires only `Cntv`) and on bare metal
`CNTVOFF_EL2` is zero. That removes the kernel-side CNTP fault, so a future
firmware that boots under WHPX would run. The bundled EDK2 still hangs, so the
smoke test's `-Accel whpx` path is still a documented failure.
## Test log (historical: milestone 0.0.5)

This section preserves the 0.0.5 evidence exactly as it was recorded. It is **not**
the current test record; the 0.0.6 QEMU evidence, including the eight-vCPU,
two-cluster, debug-invariant and ESP-image runs, is in
[`milestone-0.0.6.md`](milestone-0.0.6.md) and `build/evidence-0.0.6-*.log`.

All QEMU tests below use EDK2 AArch64 firmware, `-cpu max`, `-smp 4`, `-m 2048`,
GICv3, QEMU 10.1.94 (v10.2.0-rc4) on Windows, and the release 0.0.5 build from
2026-11-11. Results are QEMU results only.

### QEMU, normal mode

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -TimeoutSeconds 120
```

Result: PASS. Verified loader and kernel 0.0.5 banners, BootInfo v2, ACPI
checksums, relocatable ELF load at `0x00000000bba29000` (60 relocations,
11288 KiB), the higher-half map (`24 tables`, 3 GiB physmap), a clean W^X audit,
both MMU self-tests, low-half sealing, PCIe enumeration, GICv3, CNTP 100 Hz,
physical allocator and heap self-tests, four-core MADT discovery, all three
secondaries online through PSCI `CPU_ON`, 3/3 IPI round trips, per-core
scheduler completion (200 ticks / 39 switches on every core), all eight task FP
states verified, `SMP: concurrency verified`, the boot-core handoff, five
processes and six threads with 35 user pages, every `init`/IPC/thread line, the
deliberate pid 4 fault, all five pids reaped with zero user pages, the 0.0.5
success marker, and PSCI `SYSTEM_OFF`. Inventory time: 20,690 us. The host ran
firmware at EL1, so the FADT selected the HVC conduit.

Selected log lines:

```text
MMU: higher half, TTBR1 0x00000000bbd1e000, TTBR0 boot 0x00000000bb229000, 24 tables of 2048, physmap 3 GiB
MMU: W^X audit clean (no writable-executable leaf)
MMU: address-space self-test passed (ASID switch and user page)
MMU: fault self-test passed (unmapped read, read-only write, NX execute)
Physical memory: managed 1992 MiB (510196 pages), free 1992 MiB, span 0x0000000040000000-0x00000000bffff000
Physical self-test: PASS (alloc, free, alignment, invalid-free, double-free, scrub, reservations)
Heap: 512 KiB from 2 region additions, 512 KiB free in 1 blocks, largest 512 KiB, waste 0 KiB
Heap self-test: PASS (small, large, aligned, repeated, growth, growth cap, invalid-free, double-free)
SMP: 4 discovered, 4 expected online, boot MPIDR 0x0000000000000000, PSCI compliant (HVC)
IPI: 3 secondaries answered a ping/pong round trip
MMU: TTBR0 walks disabled; low half sealed
SMP: concurrency verified (every online core executed scheduler work)
Scheduler: boot core handing off to the user phase
Userspace: 5 processes, 6 threads, 35 pages, 16 KiB user stacks
init: hello from EL0
init: syscall ABI 0x00010000
ipc: receiver got ping
ipc: sender got pong
Userspace: pid 4 fault (ESR 0x0000000092000006, FAR 0x0000000000001000), terminating process
Userspace: summary 5 processes, 0 threads, 1 timer ticks
  pid 4: terminated by fault, status 139, 0 user pages
SKETCHOS 0.0.5 KERNEL OK
QEMU: requesting PSCI SYSTEM_OFF
```

### QEMU, framebuffer mode

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -Framebuffer -TimeoutSeconds 120
```

Result: PASS. Framebuffer console active at 800x600; the loader reserved a 3 MiB
GOP framebuffer, so the managed physical total is 1989 MiB instead of 1992 MiB.
All higher-half, W^X, self-test, SMP, IPI, scheduler and user-phase checks pass;
the user phase recorded two timer ticks rather than one. PSCI `SYSTEM_OFF`.
Inventory time: 110,103 us. The synchronous framebuffer console dominates this
number and it is not a kernel-speed measurement.

### QEMU, relocation mode

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -OccupyLegacy -TimeoutSeconds 120
```

Result: PASS. The loader reserves `0x40200000` before loading the kernel. The log
prints `Relocation test: occupied legacy kernel address 0x0000000040200000` and
the kernel still runs from a different base
(`Kernel entry: physical 0x00000000bba29000`), proving the boot does not depend
on the old fixed address. The map uses 25 tables. All higher-half, W^X,
self-test, SMP, IPI, scheduler and user-phase checks pass; PSCI `SYSTEM_OFF`.
Inventory time: 20,851 us.

### QEMU, EL2 entry mode

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -Virtualization -ExpectEl2 -TimeoutSeconds 120
```

Result: PASS. EDK2 enters the kernel at EL2, `boot.S` drops to EL1h, and the log
reports `Entry exception level: EL2, running at EL1`. The FADT reports the SMC
conduit in this mode (`PSCI compliant (SMC)`) and PSCI `CPU_ON` still starts all
three secondaries. All higher-half, W^X, self-test, SMP, IPI, scheduler and
user-phase checks pass; PSCI `SYSTEM_OFF`. Inventory time: 21,143 us.

### Physical laptop

Not tested. A physical test requires explicit approval, a removable USB medium, and a
Secure Boot decision. Do not alter Windows boot configuration, firmware settings, or
the internal disk. See `docs/real-hardware.md`.

## Update policy

- Add a row only when firmware describes the device or a driver exists.
- Change status to **Verified** only after a reproducible test on the named platform.
- Record the exact firmware version, build, and command used for every test result.
- Keep QEMU and laptop results separate. A QEMU pass is not a laptop pass.
- Do not claim support because a PCI ID or ACPI HID was discovered.
- Keep `docs/hardware.md` as historical context; this file is the maintained matrix.
