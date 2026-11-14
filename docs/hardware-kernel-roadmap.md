# SketchOS hardware-first kernel roadmap

Target platform: HP OmniBook X 14, Snapdragon X Plus X1P-42-100, Adreno X1-45  
Validation policy: source/spec review and cross-build first; physical laptop boot is the acceptance test. No QEMU run is required by the hardware-first build workflow.

## Current baseline

Existing code includes:
- AArch64 UEFI loader and custom BootInfo handoff
- EL1 kernel entry, custom translation tables and linear physical map
- physical page allocator and kernel heap
- GICv3/v4-style distributor/redistributor initialization
- PSCI-based SMP bring-up and per-core scheduler execution
- one-shot architectural timer
- early framebuffer console and PL011 console
- early Qualcomm GENI UART TX path for SPCR type 0x13
- basic ACPI static-table parsing and a small PCI configuration enumerator
- basic userspace/process scaffolding

These are source/build capabilities, not a claim that every subsystem works on the X1P laptop. The current candidate ISO has not passed a physical boot test.

## Hardware-first milestones

### H0 — Prove and diagnose the kernel handoff

- Keep the UEFI handoff contract explicit and versioned.
- Record inherited EL, SCTLR/HCR/CPTR/CNTHCTL and CPU feature registers before handoff.
- Keep EL2 early faults on a dedicated fatal vector; keep scheduler vectors EL1-only.
- Initialize framebuffer output before ACPI and MMU replacement.
- Provide GENI UART polling output only when SPCR and GENI firmware state validate.
- Keep all early MMIO polling bounded.
- Acceptance: on the laptop, reach a visible kernel checkpoint and identify the last completed stage.

### H1 — Memory, translation and SMP correctness

- Validate UEFI memory descriptors, BootInfo, page-table pool bounds and cache attributes.
- Validate every TTBR/TCR/MAIR transition and physical-address size against ID_AA64MMFR0.
- Keep MMIO Device mappings separate from Normal-NC framebuffer and cacheable RAM.
- Verify secondary CPU release, MPIDR affinity, per-core stacks, GIC redistributor wake-up and timer source.
- Do not enable PCI bus mastering until DMA mapping and SMMU ownership are established.
- Acceptance: all online cores run scheduler work on the laptop without memory corruption.

### H2 — ACPI AML and platform device model

- Integrate pinned uACPI 7.0.0 behind a Rust host API.
- Parse every DSDT/SSDT and extract device IDs, dependencies and resource descriptors.
- Add a device registry with ACPI HID/CID matching and typed resources.
- Add SystemMemory and PCI_Config operation-region handlers first.
- Add GPIO, I2C/GenericSerialBus and power-resource handlers only after their controllers are working.
- Do not run _STA, _INI, _PS0 or _PS3 against unimplemented operation regions.
- Acceptance: enumerate the laptop's actual platform devices and match every resource to the firmware tables.

### H3 — Qualcomm SoC foundation

Implement only after ACPI identifies exact resources:
1. QUP/GENI controller discovery and clock/power/reset dependencies.
2. TLMM GPIO controller and interrupt routing.
3. RPMh/regulator and clock-controller interfaces needed by the target devices.
4. SMMU/IOMMU setup and DMA API.
5. PCIe root complex, ECAM config, MSI/MSI-X and bus mastering behind IOMMU.
6. USB xHCI controller and DMA-safe transfer rings.

Do not copy register bases or clock IDs from SDM845/other Snapdragon generations without matching them to the X1P ACPI tables and Qualcomm documentation.

### H4 — Essential laptop I/O

- I2C HID keyboard and touchpad, using AML-described GPIO, I2C and power resources.
- USB HID keyboard/mouse/touchscreen as a separate xHCI path.
- NVMe or other storage only after PCIe/ACPI enumeration confirms the actual device.
- Battery, AC adapter, thermal sensors and power-button events.
- Acceptance: built-in keyboard, touchpad and touchscreen work; external USB input works; persistent storage can be read and written.

### H5 — Display, graphics and networking

- Keep GOP as the initial display fallback.
- Implement panel/backlight control from ACPI resources before replacing GOP.
- Adreno GPU support requires the correct Qualcomm firmware, power, clocks, SMMU and command submission; do not treat a framebuffer as a GPU driver.
- Enumerate Wi-Fi/Bluetooth devices from ACPI/PCI/USB and identify required firmware before writing a driver.
- Acceptance: stable native display, accelerated graphics and network interfaces.

### H6 — Full OS services

- VFS and filesystem drivers, block cache and storage error handling.
- Process isolation, syscall ABI, IPC, signals and robust user/kernel boundaries.
- Driver lifecycle, resource ownership, hotplug and device power management.
- Suspend/resume and thermal/power policy.
- Package manager and userland are consumers of these kernel services, not substitutes for them.

## Rules for implementation

- No guessed MMIO addresses, IRQ numbers, clock IDs, GPIO pins or I2C addresses.
- Every hardware register block must be tied to a source: ACPI resource, official SoC documentation, upstream Linux driver or vendor firmware interface.
- Every register sequence must document reset/clock/power prerequisites and timeout behavior.
- No DMA-capable device is allowed to bus-master until the IOMMU/SMMU mapping is correct.
- Unsupported ACPI operation regions return explicit errors; they never report fake success.
- The hardware-first CI builds and packages the ISO without running an emulator. Physical laptop results are recorded separately from compile/test results.


Board-specific reference: [X1P42100 HP OmniBook X14 upstream Linux cross-check](x1p42100-upstream-linux-reference.md). Use it to understand the board topology and driver sequencing, but confirm resources against the laptop's ACPI tables before touching hardware.
