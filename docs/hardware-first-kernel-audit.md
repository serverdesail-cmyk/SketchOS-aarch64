# Hardware-first kernel audit — Snapdragon X Plus

Date: 2026-11-13  
Target: HP OmniBook X 14-fe1xxx, Snapdragon X Plus X1P42100, Adreno X1-45  
Audit base: `stability/arm64-boot-audit` at `b029a719aeb0ee8ef8a69c96ad713dcd45eb0d0b`  
Status: static source/spec review in progress; **no physical boot has been verified**.

This document is deliberately evidence-led. A source-code observation is not treated as a proven hardware failure unless the relevant runtime evidence exists. QEMU is retained as a secondary regression target, not as the platform design target.

## Executive assessment

SketchOS is beyond a bootloader demo: it has an AArch64 entry path, higher-half MMU, exception vectors, ACPI table inventory, GICv3/v4-oriented code, generic timer, PSCI, SMP, physical allocator, heap, scheduler, address spaces, EL0 processes and basic syscalls.

It is **not yet a complete laptop kernel**. The hardware-specific device path is mostly inventory-only. There is no working Qualcomm platform driver stack for the target's SMMU/IOMMUs, power/clock/interconnect controllers, QUP/I2C, USB, HID, NVMe, GPU/display, Wi-Fi or audio. The current successful CI proves the QEMU virt profile only.

The first job is to make the kernel entry and early diagnostics deterministic. Only then should device drivers be brought up.

## Findings from code compared with specifications

### P0 — Early failure can be invisible on the actual laptop

**Evidence in source**
- `kernel/src/main.rs::kmain` validates BootInfo before initializing any console. On validation failure it immediately calls `arch::halt()`.
- The original path only initialized PL011 and brought up the framebuffer after ACPI/MMU. The hardware branch now initializes the GOP console before ACPI/MMU and has an output-only GENI path for SPCR type `0x13`.
- The laptop firmware inventory records SPCR interface type `0x13` at `0x894000`. ACPI defines `0x13` as the SDM845 UART interface with a 7.372 MHz clock; Qualcomm's GENI Serial Engine UART is the matching hardware family. The loader now preserves that classification. The GENI path checks the UART protocol and refuses DMA mode, but its actual X1P clock/power/firmware state remains unverified.
- The early white/green framebuffer markers in `boot.S` are useful, but they depend on the GOP framebuffer description and its mapping being valid.

**Impact**
A fault during BootInfo validation, ACPI discovery, MMU replacement, or before framebuffer-console initialization can leave only the loader's `HANDOFF: ENTERING KERNEL` text visible. This prevents us from distinguishing a bad handoff from a kernel exception.

**Required change**
1. Add a minimal, bounds-checked early GOP framebuffer writer that is usable before ACPI and before the final MMU map.
2. Preserve the last boot stage and exception syndrome in a reserved BootInfo/diagnostic page.
3. Add a panic/exception path that writes a compact status code to the framebuffer without depending on heap, scheduler, ACPI AML or interrupts.
4. **Partially implemented:** the loader now maps SPCR type `0x13` to `SDM845_GENI`; the kernel verifies GENI UART protocol and FIFO mode, then uses bounded polling TX without touching firmware clock/baud settings. Remaining work: parse/validate the complete SPCR GAS, confirm the X1P QUP clock/power state, and test the console on the laptop. Never treat this interface as PL011.



### P0 opportunity — The laptop's early debug UART has a documented hardware family

The SPCR interface code is useful evidence, not an opaque vendor value:
- ACPI interface type `0x13` means **SDM845 UART, 7.372 MHz clock**.
- U-Boot's Qualcomm debugging documentation identifies SDM845-and-newer debug UARTs as **GENI Serial Engine UARTs**, with a 7.3728 MHz reference clock and a polled early-debug implementation.
- U-Boot's GENI UART driver provides the register offsets and TX sequence; Linux's `qcom_geni_serial` driver provides the production driver reference.
- The laptop SPCR base `0x894000` matches a QUP GENI UART instance in the upstream SDM845 platform description. This is strong evidence for the IP family, but not proof that every X1P clock, reset, firmware or power-domain detail matches SDM845.

**Immediate implementation target**
Build a tiny *output-only* early console first. It should validate the SPCR GAS space/access width and the GENI protocol status, use bounded polling with a timeout, and never enable DMA or interrupts. Keep the full QUP clock/power-domain management for the later platform driver. If the firmware has not left the serial engine initialized, fail closed and retain framebuffer diagnostics rather than writing guessed register sequences.

### P1 — Timer PPI selection can disagree with the timer actually programmed

**Evidence in source**
- `kernel/src/arch/aarch64/timer.rs` programs `CNTV_CTL_EL0` and `CNTV_CVAL_EL0`: it is an EL1 virtual-timer driver.
- `kernel/src/arch/aarch64/gic.rs::timer_irq_from_platform` prefers GTDT's virtual EL1 GSIV, but falls back to the non-secure physical EL1 GSIV.
- The physical EL1 timer and virtual EL1 timer are distinct interrupt sources.

**Impact**
If firmware omits the virtual timer GSIV but supplies the physical timer GSIV, SketchOS can enable the physical timer PPI while programming the virtual timer. The timer then never delivers the interrupt expected by the scheduler. QEMU may not expose this because its GTDT fields differ from the laptop's.

**Status on this branch: source pairing implemented, hardware not tested.** The GIC now selects `(TimerKind, GSIV)` together. Virtual EL1 selects CNTV registers; non-secure Physical EL1 selects CNTP registers. A present but unusable GTDT now fails initialization instead of silently choosing a mismatched fallback. The timer driver reads and programs the matching counter/control/compare registers. Add host-side tests for each GTDT field combination and validate the laptop's exact GSIV from its raw GTDT.

### P1 — SMMU/IOMMU and DMA ownership are not implemented

**Evidence in source**
- `kernel/src/acpi/iort.rs` validates/counts IORT nodes; its own module comment says it does not program an SMMU.
- The existing hardware matrix records two Qualcomm IOMMUs and 30 IORT nodes on the laptop.
- `boot/uefi/src/dma.rs` disables PCI bus mastering through UEFI PCI root-bridge protocols, but this does not initialize or prove safe the integrated Qualcomm DMA engines or SMMU stream mappings.

**Impact**
A kernel that enables NVMe, Wi-Fi, USB or GPU bus mastering without a real DMA/IOMMU ownership model can corrupt RAM. Merely clearing PCI bus-master bits is not a substitute for an SMMU driver.

**Required change**
Implement a DMA API before enabling any bus-mastering device:
- physical/virtual address and cache-coherency rules;
- coherent and streaming mappings;
- IOVA allocator;
- SMMUv3 discovery from IORT;
- stream-ID/device mapping;
- fault reporting and device quiesce/reset.
Until then, keep bus mastering disabled and do not bind DMA-capable drivers.

### P1 — ACPI inventory exists, but ACPI device discovery is incomplete

**Evidence in source**
- `kernel/src/acpi/mod.rs` parses static tables such as MADT, GTDT, MCFG, IORT, PPTT, SRAT, SPCR and DBG2.
- The module explicitly says DSDT AML evaluation is a later milestone.
- The current laptop hardware inventory includes ACPI-described QUP/I2C devices, ELAN touchpad, QTEC keyboard, GPIO and other Qualcomm platform devices.

**Impact**
Static table decoding is enough to discover CPUs, GIC and some timers, but it is not enough to enumerate and configure many laptop devices. Their resources and power sequencing are exposed through the ACPI namespace and AML methods such as `_HID`, `_CID`, `_CRS`, `_STA`, `_PS0`, `_PS3`, `_DEP` and GPIO/I2C resource descriptors.

**Required change**
Add a bounded ACPI namespace/AML subsystem, initially read-only evaluation for device enumeration and resource extraction. Add controlled method execution only when the operation-region handlers and synchronization model are safe. Do not hard-code guessed MMIO addresses from device names.

### P1 — Qualcomm platform enablement is a stack, not a single driver

Qualcomm's upstreaming overview for Snapdragon X Elite lists SMMU, QUP (SPI/I2C/UART), interconnect, clocks, power domains, PCIe PHY, USB, NVMe, GPU/display and related components as separate enablement work. The HP laptop's firmware inventory exposes corresponding Qualcomm devices. A generic ACPI parser and PCI enumerator cannot replace those platform drivers.

**Required change**
Build a Qualcomm platform layer with explicit dependencies:
1. firmware/ACPI namespace and resource discovery;
2. clocks, resets, regulators/power domains and interconnect;
3. SMMU and DMA API;
4. PCIe root-complex support and NVMe;
5. QUP/I2C/SPI, GPIO and HID input;
6. USB host/role switching;
7. display pipeline and Adreno GPU as separate projects;
8. Wi-Fi/Bluetooth/audio and power/thermal management.

### P1 — The current loader/kernel handoff is a custom contract, not the standard Linux arm64 entry contract

**Evidence in source**
- `boot/uefi/src/handoff.rs` creates TTBR0/TTBR1 roots and passes a BootInfo pointer while leaving the active firmware translation regime in place.
- `kernel/src/arch/aarch64/boot.S` assumes MMU/caches enabled, then replaces EL1 translation state.
- Linux's documented AArch64 kernel entry contract requires the MMU off, DAIF masked, the kernel image cleaned to PoC, a defined EL1/EL2 state, and consistent timer/coherency setup.

**Impact**
This is not automatically invalid because SketchOS defines its own ABI, but it creates a larger and more fragile trust boundary: the kernel depends on firmware translation state while executing physical code, then swaps translation regimes. The existing QEMU pass does not validate the Snapdragon firmware's exact EL2/EL1 behavior.

**Required change**
Rework the boot ABI toward a minimal, explicit entry contract:
- pass only a versioned physical BootInfo pointer in a documented register;
- quiesce DMA before handoff;
- make the kernel image and handoff data visible to PoC;
- enter at a documented exception level with DAIF masked and a known cache/MMU state;
- have the kernel construct its own initial translation tables from physical addresses.
The loader may perform the necessary EL2-to-EL1 transition, but must not rely on unknown EL1 state. Preserve an EL2 path and an EL1 path with separately specified register contracts. This redesign must be reviewed against the UEFI AArch64 binding and Arm architecture requirements before implementation.

### P2 — Current PCI code is inventory, not a production PCIe host driver

**Evidence in source**
- `kernel/src/pci.rs` intentionally reads ECAM configuration space only.
- It does not assign BARs, configure bridges, route interrupts, allocate MSI/MSI-X vectors, or establish DMA mappings.
- The laptop has multiple MCFG windows and two PCIe root complexes.

**Impact**
Seeing a device ID in a log does not mean the kernel can use that device. NVMe and Wi-Fi require a complete PCIe host, interrupt and DMA path.

**Required change**
Keep enumeration read-only until host-bridge windows, BAR allocation, bridge setup, INTx/MSI routing, IOMMU mappings and reset sequencing are implemented.

## Specification set used for this audit

1. UEFI Specification 2.11, AArch64 platform requirements: <https://uefi.org/specs/UEFI/2.11/02_Overview.html>
2. Linux AArch64 boot protocol (used as a conservative, well-documented kernel-entry reference, not as a claim that SketchOS is Linux ABI compatible): <https://www.kernel.org/doc/html/latest/arch/arm64/booting.html>
3. ACPI Specification 6.6: <https://uefi.org/specs/ACPI/6.6/>
4. Arm architecture system-register descriptions for CNTHCTL_EL2 and timer access: <https://documentation-service.arm.com/static/6023d5362cb3723f202089b8>
5. Qualcomm, upstreaming Linux kernel support for Snapdragon X Elite (component-level platform enablement context): <https://www.qualcomm.com/developer/blog/2024/05/upstreaming-linux-kernel-support-for-the-snapdragon-x-elite>
6. Microsoft SPCR/DBG2 interface type table (defines `0x13` as SDM845 7.372 MHz): <https://learn.microsoft.com/en-us/windows-hardware/drivers/bringup/acpi-debug-port-table>
7. U-Boot Qualcomm early UART debugging (GENI SE, 7.3728 MHz reference): <https://docs.u-boot.org/en/latest/board/qualcomm/debugging.html>
8. U-Boot GENI UART implementation: <https://github.com/u-boot/u-boot/blob/master/drivers/serial/serial_msm_geni.c>
9. Linux Qualcomm GENI UART driver: <https://github.com/torvalds/linux/blob/master/drivers/tty/serial/qcom_geni_serial.c>

## Build plan — hardware is the acceptance platform

### Stage 0: make failure observable
- Early framebuffer diagnostics before ACPI/MMU.
- Record entry EL, SCTLR/TCR/TTBR values, MMFR/PFR feature IDs, BootInfo validation status and first exception.
- Add a bounded exception handler that reports ESR/FAR/ELR and then parks.
- Fix timer source/GSIV pairing.
- No device DMA is enabled.

### Stage 1: make entry deterministic
- Write a line-by-line boot ABI register table.
- Compare every assembly operation with the UEFI AArch64 binding and Arm register semantics.
- Replace the custom inherited-MMU handoff with a minimal known-state contract.
- Audit cache maintenance, TLB invalidation, barriers, stack, relocation and linker symbols.
- Keep tests for both EL1 and EL2 entry modes.

### Stage 2: establish hardware kernel primitives
- Memory map and page allocator reservations.
- Cache maintenance and DMA API.
- Exception and interrupt framework.
- GICv3/v4, generic timer, PSCI, SMP and per-core state.
- SMMUv3/IORT before any device bus mastering.

### Stage 3: laptop platform services
- ACPI namespace/AML and resource parsing.
- Qualcomm clocks, resets, regulators, power domains, interconnect, GPIO and QUP.
- PCIe host bridge and NVMe.
- USB host/role switching and HID.
- Display, GPU, Wi-Fi, Bluetooth, audio and thermal/power management.

### Stage 4: general-purpose kernel services
- Stable block layer, filesystems, networking, input subsystem, process/syscall ABI, scheduler, timers, virtual memory, security and user/kernel interfaces.
- Add drivers only behind stable subsystem APIs.

## Acceptance policy

- A QEMU success is a regression result, not a hardware-support claim.
- A source audit can identify invalid assumptions and specification mismatches, but cannot prove the laptop firmware's runtime state.
- Physical boot is the acceptance test for this target.
- Do not enable a DMA-capable device until its reset, interrupt and SMMU path is understood.
- Do not mark a device supported until it has been initialized and exercised on the HP OmniBook X.


### P1 — Direct EL1 entry does not establish all EL2-controlled access required by the kernel

**Evidence in source**
- The EL2 entry path clears `CPTR_EL2.TFP` and enables `ICC_SRE_EL2.SRE/Enable` before dropping to EL1.
- The direct EL1 path only sets `CPACR_EL1.FPEN`; it cannot configure `CPTR_EL2` or `ICC_SRE_EL2`.
- The Linux arm64 entry requirements explicitly require `CPTR_EL2.TFP=0` and, for GICv3 system-register mode, `ICC_SRE_EL2.SRE=1` and `ICC_SRE_EL2.Enable=1` when EL2 is present and the kernel enters at EL1.

**Impact**
The direct-EL1 path is not self-contained on a platform where EL2 exists and firmware has not already established those controls. The exception vectors save Q registers, so a trapped FP/SIMD access can fail inside the exception handler itself. GIC system-register accesses can also trap or be unavailable.

**Required change**
Specify these as explicit loader/firmware preconditions for direct EL1 entry and verify them where architecturally readable. If they cannot be guaranteed, use the EL2 entry path and perform the setup there. Do not claim the direct-EL1 path is portable until this contract is met.

### P1 — [FIXED IN THIS BRANCH] The installed EL2 vector table used an EL1-only return path

**Evidence in source**
- `boot.S` installs `exception_vectors` into both `VBAR_EL1` and `VBAR_EL2` before the EL2-to-EL1 transition.
- `vectors.S::save_system_registers` distinguishes EL1/EL2/EL3 when saving syndrome registers.
- `vectors.S::restore_frame` always writes `ELR_EL1` and `SPSR_EL1`, then executes `eret`. It is therefore an EL1 return path, not a valid EL2 exception return path.

**Impact**
A synchronous exception while still executing at EL2 can enter this table, but the common return path restores the wrong exception-level bank. This can turn an early diagnostic exception into a second fault or an uncontrolled return.

**Status on this branch: fixed.** `VBAR_EL2` now points to a dedicated 2 KiB-aligned early-fault vector table. It records `ESR_EL2`, `ELR_EL2`, `SPSR_EL2`, `FAR_EL2`, `HPFAR_EL2` and `SCTLR_EL2` into a reserved BSS record, then parks without using the EL1 stack or EL1 return registers. The general scheduler/context-switch vectors remain EL1-only. The fault path has compiled, but has not been exercised on hardware.


## Changes already made on the hardware-first branch

These are source changes, not hardware-support claims:

- Framebuffer console initialization moved to immediately after BootInfo validation, before ACPI discovery and before the kernel replaces the loader page tables.
- Framebuffer clear/scroll operations now use volatile 32-bit scanout accesses and an output barrier.
- SPCR interface `0x13` is preserved as `SDM845_GENI`; a bounded, polling-only GENI TX path verifies UART protocol and refuses DMA mode. It does not load firmware, change clocks, change baud, or manage QUP power.
- GIC timer selection now carries the selected source with its GSIV. CNTV and CNTP registers are used only with their corresponding GTDT interrupt field.
- Added a separate `hardware-kernel-build.yml` workflow that builds the AArch64 kernel/UEFI ISO, runs host-side allocator/scheduler tests, checks changed Rust formatting and uploads an ISO **without running an emulator**.

- The UEFI loader now prints the inherited entry EL, `CNTFRQ_EL0`, ID register values and EL-specific SCTLR/HCR/CPTR/CNTHCTL state before handoff; it avoids probing GIC system registers that firmware may have gated.
- The UEFI SPCR parser now checks the table checksum and validates the SystemMemory GAS geometry before exposing a 32-bit PL011/GENI base.
- The framebuffer is not marked active until its first bounded volatile clear completes, preventing an early framebuffer fault from recursively printing through the same invalid mapping.
- EL2 early faults now use a dedicated fatal vector path rather than the EL1 context-switch exception table.

None of these changes has been tested on the HP laptop yet. The next static pass is to validate SPCR GAS details, review the GENI sequencer protocol against the Qualcomm/Linux sources, and audit every EL1/EL2 entry register against the UEFI and Arm contracts.


### P0 — QEMU VirtIO-MMIO probing was unconditional on physical hardware

**Evidence in source**
- `main.rs` called `storage::probe()` and `net::probe()` unconditionally after heap initialization.
- Both drivers scanned the QEMU virtio-mmio aperture beginning at physical `0x0a000000` in 0x200-byte steps.
- The storage probe did more than read: it wrote the VirtIO status, configured a queue PFN and set DRIVER_OK, allowing device DMA.
- This address range is a QEMU `virt` machine convention, not a generic AArch64 bus. The Snapdragon laptop must use firmware-described ACPI/PCI resources instead.

**Impact**
On the laptop, those reads can hit an unpowered or unmapped MMIO region and cause an external abort. If a coincidental device signature were read, the storage probe could configure DMA without an SMMU mapping. This is a concrete hardware-safety defect and a plausible cause of a post-handoff failure, although it is not yet proven to be the exact failure point.

**Status on this branch: fixed.**
- The functions are now explicitly named `probe_qemu_virtio_mmio`.
- `main.rs` calls them only when `FLAG_QEMU_VIRT` is set.
- On physical hardware, both probes are skipped and reported as unavailable until ACPI/PCI device discovery and real drivers exist.

### P1 — PCI ECAM inventory could fault on malformed or powered-down windows

**Evidence in source**
- MCFG parsing previously accepted any nonzero base and bus range without checking the 1 MiB ECAM alignment or whether the complete bus window fit inside the kernel's 512 GiB physical map.
- PCI inventory used raw volatile loads. A valid MCFG descriptor does not guarantee that a particular root complex is powered at the moment of probing.

**Status on this branch: hardened.**
- MCFG entries now require a 1 MiB-aligned base and a checked full-window end within the supported physical map.
- ECAM reads use the kernel's single-core, fault-contained 8/16/32-bit probe helpers.
- A window whose first configuration read aborts is skipped rather than repeatedly faulted.
- These probes are still inventory-only and read-only; PCI bus mastering remains disabled until SMMU/DMA support exists.


### P1 — ACPI parsers needed stronger bounds and structure validation

The source review found that several table decoders trusted firmware-declared offsets more than necessary:
- The kernel's raw ACPI reader did not ensure each table/RSDP range fit inside the current 512 GiB physmap before constructing a slice.
- MADT decoding checked the enclosing table bounds but did not check each known entry's minimum structure length before reading GICC/GICD/GICR/ITS fields.
- IORT node counting scanned to the end of the table rather than honoring the declared node count, and accepted undersized node headers.
- SPCR/BGRT/TPM2 decoders did not all enforce their complete fixed payload lengths.

**Status on this branch: hardened.** The reader bounds every firmware table range to the physmap; MADT checks the ACPI-defined minimum entry sizes; IORT uses the declared node count and minimum node header size; SPCR validates the GAS geometry and physical range, while BGRT/TPM2 require their fixed payload lengths. These changes are compile-validated, not a substitute for reviewing the actual laptop table contents.


### P1 — Unsynchronized mutable globals in storage, filesystem, network and display scaffolding

The previous implementation exposed references into `static mut` globals for the VirtIO block device, FAT32 filesystem, network device and DRM scaffold. Returning `&'static mut` from a global does not establish exclusive access and can create Rust aliasing undefined behavior when more than one caller exists.

**Status on this branch: removed from the active scaffolding.**
- Block-device state and network/display state now use the kernel IRQ spinlock.
- Partition metadata is built locally and published under a lock instead of mutating global arrays without synchronization.
- FAT32 mount now returns an owned, read-only volume object; the old global mutable filesystem reference and unimplemented write path are removed.
- The experimental ext2 formatter is explicitly named as a QEMU test-disk formatter and refuses physical-hardware use.


### P1 — The timer silently defaulted to PPI 27 when GTDT was missing

The GIC code previously fell back to the architectural virtual timer PPI 27 whenever no GTDT timer entry was parsed. That is a QEMU-friendly default, not a valid hardware discovery policy for the Snapdragon laptop. A missing or malformed GTDT could therefore select an interrupt that firmware never assigned to the timer.

**Status on this branch: fixed.** The fallback to PPI 27 is now allowed only when the UEFI loader explicitly marks the machine as QEMU virt. Physical hardware must provide a valid GTDT timer entry. The selected EL1 timer's GTDT flags must describe the supported level-sensitive, active-high configuration, and its GSIV must be in the GIC PPI range 16–31.
