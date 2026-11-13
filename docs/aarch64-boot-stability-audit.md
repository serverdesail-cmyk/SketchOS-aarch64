# AArch64 boot stability audit

Date: 2026-11-13
Branch: `stability/arm64-boot-audit`
Scope: UEFI loader, ELF loader, boot-info ABI, AArch64 entry assembly, page-table builder, early kernel initialization, and CI.

## Scope clarification

SketchOS is a freestanding AArch64 operating system with a custom UEFI loader and Rust kernel. It is not a Linux kernel source tree. Linux's arm64 boot protocol is therefore used as an architectural reference, not as a claim that SketchOS is Linux ABI-compatible.

The primary reference is the [Linux arm64 boot protocol](https://docs.kernel.org/arch/arm64/booting.html). It specifies masked interrupts, MMU-off entry, kernel image cache maintenance to the point of coherency, consistent timer state, CPU coherency, and initialized architectural system registers. SketchOS currently uses a custom handoff with its own BootInfo and translation roots. Unlike Linux's standard entry contract, the loader currently enters the kernel with its bootstrap MMU/cache regime enabled; the kernel then clears BSS, cleans those cache lines, disables EL1 translation/cache state, installs its own roots, and re-enables the regime. This is an intentional custom protocol, not Linux arm64 boot-protocol compliance, and must remain explicit and covered by regression tests.

## Findings addressed in this branch

### 1. EL1 SCTLR state was inherited from firmware

The old entry path read-modify-wrote `SCTLR_EL1`, preserving an unknown register state when firmware entered at EL2. That is not a safe basis for an OS transition: the Linux arm64 boot contract explicitly calls out system-register state as something the loader must initialize.

The new path writes a deterministic EL1 baseline using the Linux arm64 RES1 bit set (`0x30500800`), with MMU/data-cache/instruction-cache disabled while replacing translation registers, then enables the new regime. It also initializes EL1 FP/SIMD access and exception vectors before the EL transition.

### 2. Early BSS writes could remain dirty when the data cache was disabled

The kernel clears `.bss` before changing EL1 translation state. The previous code then disabled the data cache without explicitly cleaning those just-written cache lines. On a non-coherent transition, this risks losing the zero initialization that the kernel relies on for its page tables and static state.

The new entry path reads `CTR_EL0.DminLine`, aligns the BSS start down to the data-cache line, issues `DC CIVAC` for the complete BSS range, and completes the operation with `DSB SY` before clearing `SCTLR_EL1.C`.

The loader already cleans the loaded kernel image, boot-info record, final memory map, and bootstrap page-table pool before entering the kernel.

### 3. PCI bus mastering could continue DMA into kernel-owned RAM

Linux's arm64 boot protocol explicitly requires DMA-capable devices to be quiesced before kernel entry. The loader previously left PCI bus-master enable bits untouched, allowing a firmware-enabled device to continue writing into RAM after the kernel began allocating it.

The loader now enumerates UEFI PCI root bridges, walks their PCI topology, and clears the PCI Command register's Bus Master Enable bit on discovered functions after all file and firmware I/O has completed. Failures are counted and reported. This covers PCI devices exposed through UEFI root-bridge protocols; it does not yet prove that every SoC-integrated/non-PCI DMA engine or IOMMU is quiesced on the Snapdragon platform.

### 4. The final UEFI memory map could be silently truncated

The loader allocated a descriptor array with slack, but silently stopped copying if the final UEFI map exceeded that capacity. A truncated map can cause the kernel to misclassify reserved memory or miss RAM ranges. The handoff now fails closed instead of passing a partial map to the kernel.

UEFI requires the loader to use the current memory-map key for `ExitBootServices()` and to retry `GetMemoryMap()` plus `ExitBootServices()` if the key is stale. See [UEFI 2.10, ExitBootServices](https://uefi.org/specs/UEFI/2.10/07_Services_Boot_Services.html).

## Existing architecture reviewed

- ELF64 AArch64 loading and `R_AARCH64_RELATIVE` relocation.
- UEFI memory-map normalization and `ExitBootServices()` handoff.
- BootInfo version/size fields and translation-root metadata.
- EL2-to-EL1 and direct EL1 entry paths.
- Primary/secondary CNTVOFF setup when entering through EL2, plus CNTHCTL access policy.
- UEFI PCI root-bridge enumeration and PCI Command.BusMasterEnable clearing.
- TTBR0 bootstrap identity map and TTBR1 higher-half/physical map.
- TCR/MAIR setup and page-table descriptor attributes.
- Kernel W^X segment mapping and low-half sealing.
- Exception vector alignment and trap-frame ABI.
- Physical allocator reservations, heap, address-space and fault self-tests.
- PSCI secondary CPU startup, GICv3, timer, and scheduler test paths.

## Validation added

The CI workflow now intends to run:

1. `cargo fmt --all -- --check`.
2. Host-side unit tests for `sketchos-mm` and `sketchos-sched-core`.
3. A locked release build of the UEFI loader, kernel, userspace, and ISO.
4. A QEMU AArch64 UEFI boot of the generated ISO.
5. Serial-log assertions for loader handoff, kernel entry, higher-half MMU, W^X, address-space and fault self-tests, userspace process cleanup, and QEMU power-off.

The QEMU test is a regression gate, not proof of Qualcomm hardware support. The existing repository has legacy Rust files that do not pass a whole-workspace rustfmt check; this branch therefore enforces rustfmt on changed Rust files rather than rewriting unrelated code in a boot-fix patch.

The boot assembly now paints an **8×8 white framebuffer checkpoint** immediately after entering EL1 with the new MMU active, then a **16×16 green checkpoint** after branching into the higher half. On the laptop: no white marker means failure before EL1 entry; white but no green means failure during the higher-half transition; green means the kernel reached the higher-half checkpoint (but not necessarily Rust `kmain`).

## Known limitations / release blockers

- No physical HP OmniBook X test has been completed by this audit.
- PCI DMA quiescence covers functions reachable through UEFI PCI root-bridge protocols; Qualcomm SoC DMA engines and IOMMU/SMMU state still need platform-specific verification.
- The current release build emits 37 Rust warnings, mostly unused/incomplete code plus five references to `static mut`. These need a separate safety cleanup before describing the whole kernel as production-stable.
- No Qualcomm-specific UART, interrupt-controller, storage, USB, I2C, GPIO, touchscreen, keyboard, touchpad, Wi-Fi, or Adreno display/GPU driver has been validated on the laptop.
- QEMU's `virt` machine is not a model of the Snapdragon X platform.
- The current kernel has a custom MMU-on UEFI handoff and does not claim Linux arm64 Image/DT boot-protocol compatibility.
- A passing build and QEMU smoke test cannot guarantee operation on every AArch64 implementation.

## Release acceptance

Do not label a release hardware-stable until all of the following are true:

- Formatting and host tests pass.
- The release ISO builds reproducibly from a clean checkout.
- The ISO completes the QEMU UEFI smoke test, including the full userspace test phase and clean shutdown.
- At least three cold boots on the target HP OmniBook X reach the kernel's success marker without reset or hang.
- The target's actual ACPI tables, memory map, exception level, timer, GIC, and framebuffer are captured and reviewed.
- A recovery path is tested before changing firmware boot order or installing to internal storage.

A claim of "guaranteed to work" is not technically supportable without defining the supported hardware/firmware matrix and testing every supported configuration.

## Reference documents

- [Linux AArch64 boot protocol](https://docs.kernel.org/arch/arm64/booting.html) — entry exception level, MMU/cache state, DMA quiescence, timers, coherency and system-register requirements.
- [Linux arm64 `SCTLR_EL1_RES1` definition](https://kernel.googlesource.com/pub/scm/linux/kernel/git/torvalds/linux/%2B/3c730b1041aefa2a92b96fcba9db237d28585922/arch/arm64/include/asm/sysreg.h) — the RES1 mask used for the EL1 baseline in this branch.
- [UEFI 2.10 Boot Services](https://uefi.org/specs/UEFI/2.10/07_Services_Boot_Services.html) — memory-map key and `ExitBootServices()` requirements.
- [UEFI Rust crate 0.41 boot services](https://docs.rs/uefi/0.41.0/uefi/boot/fn.exit_boot_services.html) — the API used by the loader to obtain the final map and exit Boot Services.
