# SketchOS 0.0.2 repository audit

Date: 2026-11-09
Scope: entire repository at `C:\Users\sailo\OneDrive\Documentos\Default Project\SketchOS`
Method: source inspection, clean release build, unit tests, and both existing QEMU smoke tests.
No large code changes were made before this report.

> **Historical note (added 2026-11-10).** This audit describes the 0.0.2 tree and
> is preserved as a historical record. Milestone 0.0.3 subsequently added the
> physical allocator, kernel heap, SMP discovery and PSCI `CPU_ON`, per-core GIC
> and timer initialization, SGI IPIs, per-core scheduling with guarded stacks,
> and the missing framebuffer, relocation and EL2 smoke-test modes. Statements
> below that describe those features as missing are accurate for 0.0.2 only. See
> [milestone-0.0.3.md](milestone-0.0.3.md) for the current verified state.

## 1. Executive summary

SketchOS 0.0.2 is a small, coherent, single-core AArch64 bring-up kernel. It boots
under UEFI, validates ACPI, installs its own identity-mapped page tables, enumerates
PCIe through ECAM, initializes a GICv3/v4 CPU interface, runs a 100 Hz generic timer,
and demonstrates preemptive round-robin context switching between four kernel threads.
All QEMU smoke-test modes pass, including an EL2 entry and a deliberately occupied
legacy load address.

It is **not** yet an operating system that can boot the HP OmniBook X:

1. **There is no laptop serial driver.** The laptop SPCR describes interface `0x13`
   at `0x894000`, which is not a PL011. Early output before the framebuffer console
   is initialized is lost.
2. **No physical allocator, heap, userspace, SMP, driver binding, DMA, IOMMU, or
   hardware driver exists.** The laptop hardware matrix is almost entirely
   unimplemented.
3. **The firmware handoff and load-address blockers are now resolved in QEMU.** The
   kernel drops from EL2 to EL1 and is a relocatable `ET_DYN` image whose base the
   loader chooses below 4 GiB. Neither has been exercised on the laptop yet.

The foundation is reasonable, but several early decisions will become expensive if
they are not corrected before drivers and SMP are added: the identity map, the
QEMU-gated power path, the GICD bit layout assumption, and the fact that the driver
API is populated from a static inventory rather than from real PCI/ACPI devices.

## 2. Repository and architecture

### 2.1 Workspace

| Crate | Path | Role |
| --- | --- | --- |
| `sketchos-bootinfo` | `boot/protocol` | Shared `no_std` boot ABI, version 1, 208-byte `BootInfo`, 32-byte `MemoryDescriptor` |
| `sketchos-uefi` | `boot/uefi` | UEFI loader (`BOOTAA64.EFI`) |
| `sketchos-kernel` | `kernel` | Freestanding AArch64 kernel |
| `sketchos-driver-api` | `drivers` | Allocation-free driver vocabulary and lifecycle trait |

The workspace has no userspace, filesystem, graphics stack, or compatibility layer
implementation. `compat/linux`, `graphics`, `userspace`, `sketchy`, `libraries`,
`tests`, and `config` contain README placeholders only.

### 2.2 Layering today

```
UEFI firmware
  └─ BOOTAA64.EFI  (boot/uefi)
       ├─ ACPI RSDP/XSDT discovery and checksum validation
       ├─ GOP framebuffer description
       ├─ ELF64 kernel load: relocatable ET_DYN placed at a free base below 4 GiB
       ├─ R_AARCH64_RELATIVE relocations applied
       ├─ BootInfo + normalized UEFI memory map (EfiLoaderData)
       └─ ExitBootServices
            └─ kernel.elf  (_start in kernel/src/arch/aarch64/boot.S)
                 ├─ arch/aarch64: boot, vectors, MMU, GIC, timer, PSCI
                 ├─ acpi: RSDP/XSDT walk and table decoders
                 ├─ memory, memmap, cpu: inventory and diagnostics
                 ├─ pci: ECAM enumeration
                 ├─ drivers: static driver-API inventory
                 ├─ console, fb, font8x8: PL011 and GOP text console
                 └─ sched: four-task round-robin demo
```

Architecture-specific code is concentrated in `kernel/src/arch/aarch64`. ACPI,
PCIe, memory reporting, and the scheduler are generic modules that currently assume
AArch64/EL1 indirectly. There is no explicit platform-profile abstraction: platform
facts are carried in the `acpi::Platform` struct and a single `FLAG_QEMU_VIRT` bit.

### 2.3 Boot protocol

`boot/protocol/src/lib.rs` defines:

- `BOOT_INFO_MAGIC = "SKBOOT01"`, `BOOT_INFO_VERSION = 1`, `BOOT_INFO_SIZE = 208`.
- `MemoryDescriptor` = 32 bytes; `MemoryRegion` = 24 bytes; `FramebufferInfo` = 32 bytes.
- Flags for memory map, RSDP, framebuffer, UART, identity map, and QEMU virt.
- Compile-time size assertions at the bottom of the file.

The kernel re-validates the magic, version, size, memory-map pointer/alignment/entry
size/count, kernel range, UART description, framebuffer geometry, and RSDP non-null
(`kernel/src/bootinfo.rs`). It does **not** verify that the memory-map array or
`BootInfo` itself lies inside a RAM descriptor before dereferencing them. The loader
is trusted, but a firmware or loader defect would fault rather than produce a
diagnostic.

## 3. Boot flow, step by step

### 3.1 Loader

`boot/uefi/src/main.rs`:

1. `uefi::helpers::init()`, disables the watchdog.
2. Prints loader banner, firmware vendor, firmware revision, loader image range.
3. `acpi::discover()` reads the RSDP from the UEFI configuration table (never a
   BIOS scan), validates the 20-byte and extended checksums, walks XSDT/RSDT, and
   extracts SPCR.
4. Builds `BootInfo`, including firmware vendor, revision, RSDP address, OEM ID,
   UART description, and GOP framebuffer.
5. If QEMU OEM ID is `BOCHS ` and SPCR is missing, falls back to PL011 at
   `0x09000000`.
6. Loads `\EFI\SketchOS\kernel.elf` from the boot volume.
7. `handoff::enter()` allocates the boot-info page and descriptor array as
   `EfiLoaderData`, calls `ExitBootServices`, copies the final map, and branches to
   the kernel entry with the boot-info physical address in `x0`.

`boot/uefi/src/elf.rs` accepts ELF64, little-endian, `EM_AARCH64`, `ET_EXEC` or
`ET_DYN`, with `PT_LOAD` segments, `filesz <= memsz`, a span <= 128 MiB, and an entry
point inside the loaded range. `ET_EXEC` images keep `p_vaddr == p_paddr` and are
loaded at their linked address. `ET_DYN` images are linked at virtual address zero:
the loader reserves the image with `AllocateType::MaxAddress(0xffff_ffff)`, applies
`R_AARCH64_RELATIVE` relocations from `PT_DYNAMIC`, and enters at
`load_bias + e_entry`. The whole span is zeroed, file-backed bytes copied, `.bss`
tails zeroed, and the instruction cache synchronized.

`boot/uefi/src/handoff.rs` allocates a descriptor array with 256 entries of slack,
exits boot services, copies the final map, and patches the count and flag. The final
map is **silently truncated** if it has more than `capacity` entries
(`handoff.rs:88-92`).

### 3.2 Kernel entry

`kernel/src/arch/aarch64/boot.S`:

- Masks DAIF.
- Reads `CurrentEL` and saves it for the kernel diagnostic.
- Clears `.bss` (including the early stack and the static page-table pools).
- At EL2, configures `HCR_EL2` (AArch64 EL1, no VHE, no traps), `CNTHCTL_EL2`
  (`EL1PCTEN | EL1PCEN`), `CNTVOFF_EL2 = 0`, clears `CPTR_EL2` trap bits, sets
  `ICC_SRE_EL2` when the GIC system-register interface exists, copies the firmware
  EL2 translation regime to EL1 when its MMU was on, and `eret`s to EL1h.
- At EL1, enables FP/SIMD with a read-modify-write of `CPACR_EL1`.
- Installs `VBAR_EL1` before entering Rust and sets `sp = __stack_top`.
- Calls `kmain(x0, x1)` where `x0` is the BootInfo address and `x1` the entry EL.

`kmain` (`kernel/src/main.rs`):

1. Validates `BootInfo`; on failure halts with no diagnostic.
2. Installs exception vectors.
3. Initializes PL011 only if the boot info says `PL011`.
4. Reads `CNTFRQ_EL0` and `CNTVCT_EL0`.
5. Decodes ACPI.
6. Builds and installs kernel page tables (failure is non-fatal).
7. Initializes the GOP framebuffer console.
8. Prints inventory: kernel, loader, UART, framebuffer, ACPI, CPU, memory, PCIe,
   driver API devices, UEFI memory map.
9. Requires ACPI; initializes GIC, timer, scheduler; any failure is fatal.
10. Starts the scheduler demo.

## 4. Completed functionality

The following is **QEMU-verified** unless marked otherwise.

| Area | Implemented | Evidence |
| --- | --- | --- |
| UEFI loader | Yes | Loader banner `SketchOS UEFI loader 0.0.2 (AArch64)` |
| ACPI RSDP/XSDT | Yes, checksum validated | `ACPI RSDP ... checksum ok`, `ACPI XSDT ... checksum ok` |
| ACPI table walk | Yes, all tables checksum-counted | `ACPI ... entries, checksum ok` |
| ACPI decoders | MADT, FADT, GTDT, MCFG, SPCR, DBG2, BGRT, TPM2, SRAT, SLIT, PPTT (counts), IORT (counts) | Inventory output |
| ELF64 kernel load | Yes, relocatable `ET_DYN`, loader-chosen base below 4 GiB | `Kernel image: ET_DYN, 26 relocations`; `Kernel allocation: ... (relocatable, bias ...)` |
| `ExitBootServices` handoff | Yes | Kernel runs after firmware handoff; map descriptors are loader data |
| Boot-info validation | Yes | Kernel banner and inventory |
| EL2-to-EL1 drop | Yes | `Entry exception level: EL2, running at EL1` with `-Virtualization -ExpectEl2` |
| Relocation safety | Yes | `-OccupyLegacy` occupies `0x40200000` and the kernel still boots elsewhere |
| Exception vectors | Installed at EL1 | `Exceptions: VBAR_EL1 0x..., table aligned: yes` |
| Kernel page tables | Yes, 4 KiB granule, 48-bit VA, identity map | `MMU: enabled, 4K granule, identity map` |
| Device vs Normal attributes | Yes, Device-nGnRnE, Normal WB, Normal NC | `mmu.rs` |
| PCIe ECAM enumeration | Yes, BFS from MCFG windows | `PCIe: 2 functions on 1 buses across 1 segments` |
| GICv3/v4 distributor + redistributor + CPU interface | Yes, boot CPU | `GIC: GICv3+ distributor 0x08000000, redistributor 0x080a0000, timer PPI 30` |
| Generic timer | Yes, CNTP at 100 Hz | `Timer: CNTP 100 Hz` |
| Preemptive scheduler | Yes, four kernel threads, round-robin | `Scheduler: demo complete, 200 ticks, 39 context switches` |
| FP/SIMD context | Yes, `q0..q31`, FPSR, FPCR saved/restored | FP bits printed per task |
| Framebuffer text console | Yes, 32-bpp GOP modes | `Framebuffer console: active` with `-Framebuffer` |
| PSCI SYSTEM_OFF | Yes, QEMU only | `QEMU: PSCI SYSTEM_OFF` |
| Driver API | Vocabulary and unit tests only | `cargo test -p sketchos-driver-api`: 3 passed |

## 5. Missing functionality

### 5.1 Execution levels and exceptions

- EL2-to-EL1 drop is implemented and QEMU-verified. EL3 is not handled.
- `install_exception_vectors` writes `VBAR_EL2/EL3` only if the current EL permits,
  but the rest of the kernel does not support running there.
- `vectors.S` always restores through `ELR_EL1`/`SPSR_EL1`, so it cannot return to a
  lower EL or an EL2 kernel correctly.
- No recoverable synchronous exception handling; synchronous exceptions, FIQ, and
  SError are fatal.
- No page-fault handler, no fault address decoding beyond printing ESR/FAR.
- No stack guard pages and no exception stack selection.

### 5.2 SMP

- No PSCI `CPU_ON`, no secondary entry point, no per-core stacks.
- No per-core data (`TPIDR_EL1`/`TPIDR_EL0`), no per-core GIC redistributor
  initialization, no per-core timer.
- No IPIs/SGIs, no spinlocks, no atomics-based synchronization, no TLB shootdown.
- Scheduler is single-core and global.
- QEMU runs with `-smp 4`, but only the boot CPU is initialized.

### 5.3 Memory management

- No physical page allocator; the UEFI map is reported, not consumed.
- No kernel heap; no `alloc` in the kernel.
- No virtual memory API (map/unmap/protect), no page permissions, no W^X.
- No user/kernel address-space separation.
- No TLB maintenance API beyond the one-time `tlbi vmalle1`.
- No memory accounting.
- Page tables live in fixed BSS pools (`MAX_L2_TABLES = 512`, `MAX_L3_TABLES = 128`);
  no dynamic table allocation.

### 5.4 Interrupts and timers

- No interrupt registration/dispatch table; only the timer PPI is handled, all other
  interrupts are acknowledged and ignored (`sched.rs:220-230`).
- No MSI/MSI-X, no GIC ITS initialization, no LPI support.
- No GICv2 support.
- No per-CPU redistributor bring-up.
- No interrupt priority grouping policy beyond `PMR = 0xff`, `BPR1 = 0`.
- No monotonic timekeeping API; only raw `CNTVCT_EL0` and a 100 Hz tick.
- Timer uses `CNTP_*_EL0` but reads `CNTVCT_EL0`; this is inconsistent if firmware
  programmed a non-zero `CNTVOFF_EL2`.
- No watchdog, no RTC, no GTDT memory-mapped timer frame support.

### 5.5 Drivers and devices

- No driver is bound to any device.
- The driver API is populated from a hand-written static inventory in
  `kernel/src/drivers/mod.rs`; PCI functions discovered by `pci.rs` are not turned
  into `DeviceId::Pci` entries, and the ACPI namespace/AML is not walked.
- Many `MmioRegion` lengths are zero, so the resources are not usable by a real driver.
- No BAR mapping, no PCI config writes, no MSI-X programming.
- No DMA API, no IOMMU/SMMUv3 programming, no cache-coherency model.
- No firmware loading, no power-domain/clock/reset framework, no device lifecycle
  beyond `matches/probe/start/stop`.

### 5.6 Userspace and system services

- No EL0 execution, no syscall interface, no process/thread model.
- No filesystem, no block layer, no network stack.
- No init process, no shell, no desktop.
- No IPC, no timers/sleep for tasks, no task termination/wait.

### 5.7 Platform abstraction

- No `Platform` trait or profile module separating QEMU virt, the OmniBook X, and
  future ARM64 platforms.
- `FLAG_QEMU_VIRT` is used not only for diagnostics but to decide whether to attempt
  PSCI power-off (`main.rs:225-232`, `sched.rs:198-205`). This conflates platform
  identity with capability.
- Missing non-essential devices generally do not block boot, but missing ACPI is fatal
  (`main.rs:210-213`), and an unusable UART leaves the laptop silent until the
  framebuffer console starts.

## 6. Hardware compatibility gaps

The detailed, maintained matrix is in
[`hardware-compatibility.md`](hardware-compatibility.md). Summary:

| Device | Identifier | Status | Why it does not work |
| --- | --- | --- | --- |
| Adreno X1-45 GPU | ACPI `QCOM0D17` | Unimplemented | No GPU, IOMMU, firmware, or power-domain driver |
| Samsung NVMe | PCI `144D:A80D` | Unimplemented | No PCIe driver, BAR mapping, DMA, MSI-X/ITS |
| Qualcomm Wi-Fi | PCI `17CB:1103` | Unimplemented | No PCIe driver, firmware loading, DMA, IOMMU |
| Qualcomm Bluetooth | ACPI `QCOM0C6B` | Unimplemented | No UART/PCIe transport or firmware loading |
| ELAN touchpad | ACPI `ELAN010E` | Unimplemented | No I2C HID or ACPI namespace/AML |
| QTEC keyboard | ACPI `QTEC0001` | Unimplemented | No input driver or ACPI namespace |
| Qualcomm audio | ACPI `QCOM0C1B` | Unimplemented | No audio DSP, clock, or power-domain driver |
| Qualcomm USB | ACPI `QCOM0D08`, `QCOM0C8B`, `QCOM0C8C` | Unimplemented | No XHCI driver, no USB stack |
| Internal panel | 2240x1400 | Unimplemented | Only GOP framebuffer; no DSI/panel driver |
| Battery/thermal | ACPI namespace | Unimplemented | No AML/EC/thermal driver |
| Qualcomm IOMMU | ACPI `QCOM068F` (two) | Unimplemented | IORT is counted only; no SMMUv3 programming |
| Serial console | SPCR interface `0x13` at `0x894000` | Blocked | Not a PL011; kernel refuses to guess |
| PCIe root complexes | ACPI `PNP0A08` (two) | Enumerated only | ECAM walk only; no resource assignment |
| TPM 2.0 | TPM2 control area `0x81f10000` | Decoded only | No TPM command interface |
| BGRT image | `0xcfdaa018` | Decoded only | Not displayed or used |

"Discovered" means the firmware describes the device. It is not a support claim.

## 7. Performance bottlenecks

No performance claim is made. The following are code-level observations, not
benchmark results.

### 7.1 Framebuffer console is the dominant boot cost

`kernel/src/fb.rs`:

- `draw_glyph` calls `fill_rect` for every font pixel; `fill_rect` calls `put_pixel`
  for every output pixel.
- `put_pixel` loads six `AtomicU32`/`AtomicU64` values and performs bounds checks for
  every pixel (`fb.rs:271-295`).
- At `SCALE = 2`, one character is 8×8×4 = 256 pixel writes, each with atomic loads.
- Output is written synchronously to Normal Non-cacheable framebuffer memory.
- `scroll()` copies whole text rows through `ptr::copy`.

Observed in QEMU: inventory time was **32,905 us** without a framebuffer and
**151,006 us** with `-device ramfb` during the audit. Later verification runs
measured 47–50 ms without a framebuffer and 122–260 ms with it. The difference is
almost entirely console output. This is a boot-time and responsiveness problem, not
a rendering architecture.

### 7.2 MMU maps the entire low physical range as Device, then promotes RAM

`mmu::init` calls `map_range(0, highest, ATTR_DEVICE)` before promoting RAM
(`mmu.rs:145-163`). `highest` is the maximum end of *all* UEFI descriptors, including
MMIO descriptors. On a laptop whose UEFI map describes high ECAM or MMIO ranges, this
can create a large number of 2 MiB block mappings and L2 tables before the useful
mappings are installed. It also maps the null page, so null dereferences do not fault.

### 7.3 Entire ECAM windows are mapped eagerly

`map_platform_devices` maps every MCFG window in full (`mmu.rs:443-456`). The laptop
describes three 256-bus windows and five small windows. Each 256-bus window is
256 MiB of Device mapping. Only the configuration space actually touched by
enumeration needs to be mapped; the rest is address-space and TLB pressure.

### 7.4 Page-table pool pressure

The MMU uses 128 static L3 tables. Each fragmented RAM descriptor edge can consume
one L3 table. A laptop UEFI memory map with more than 128 fragmented edges will make
`mmu::init` fail and leave the kernel on the firmware map. The failure is reported,
but the kernel then continues with reduced guarantees.

### 7.5 Scheduler demo busy-spins

All four tasks spin on FP arithmetic and volatile counters (`sched.rs:142-171`).
There is no idle task, no `WFI`, no sleep/wake, and no priority or affinity. On a
laptop this would burn power and generate heat for two seconds before halting. It is
a demonstration, not a scheduler.

### 7.6 Synchronous, unbuffered console output

`console.rs` writes one byte at a time to PL011 and mirrors every byte to the
framebuffer. `println!` is called per line and per table entry. There is no ring
buffer, no batching, and no lock. On the laptop, where the framebuffer is the only
console, every diagnostic line is paid for in pixel writes.

### 7.7 Other costs

- ACPI table walk prints every table with formatted output.
- PCIe enumeration reads vendor/class/header/BAR registers through Device memory.
- No per-CPU caches, no allocator, no slab, no deferred work.
- No PMU or timestamp instrumentation beyond `CNTVCT_EL0`.

## 8. Security and reliability concerns

Severity: **P0** = prevents laptop boot or can corrupt state; **P1** = correctness or
isolation gap that must be fixed before drivers/userspace; **P2** = hardening or
robustness issue.

| Sev | Finding | Location | Impact |
| --- | --- | --- | --- |
| P0 | ~~Fixed physical link address `0x40200000`; loader refuses relocation~~ **Fixed** | `kernel/linker/kernel.ld`, `boot/uefi/src/elf.rs` | Kernel is `ET_DYN`; loader picks a free base below 4 GiB and applies relocations |
| P0 | ~~No EL2-to-EL1 drop; MMU/GIC require EL1~~ **Fixed** | `kernel/src/arch/aarch64/boot.S` | EL2 is dropped to EL1h before `kmain`; QEMU `virtualization=on` test passes |
| P0 | ACPI tables with bad checksums are still parsed | `acpi/mod.rs:407-436` | Corrupt firmware data can be interpreted as valid |
| P0 | Final UEFI memory map silently truncated | `boot/uefi/src/handoff.rs:88-92` | Future allocator may miss reserved/usable regions |
| P1 | No W^X; Normal RAM is executable at EL1 and `SCTLR_EL1.WXN` is cleared | `mmu.rs:469-485`, `mmu.rs:547` | Stacks and data can execute; weakens fault isolation |
| P1 | Null page and all low physical memory are mapped | `mmu.rs:148` | Null/wild pointer accesses may not fault |
| P1 | No stack guard pages; 64 KiB boot stack and 16 KiB task stacks in BSS | `kernel.ld:38-43`, `sched.rs:29-39` | Overflow silently corrupts adjacent memory |
| P1 | GICD_CTLR bit layout assumes one security-state view | `gic.rs:25-34`, `gic.rs:98-104` | Wrong group enable on a DS=0 GICv4 laptop |
| P1 | Timer PPI validation accepts INTIDs below 16 | `gic.rs:79-82`, `gic.rs:126` | Underflow in `(timer_irq - 16)` and wrong SGI/PPI programming |
| P1 | `find_redistributor` skips a frame whose `GICR_TYPER` is zero | `gic.rs:276-279` | Could fail to find a valid affinity-0 redistributor |
| P1 | Driver API is not populated from PCI/ACPI namespace | `kernel/src/drivers/mod.rs` | No path from discovery to a real driver |
| P1 | MMIO lengths are zero for many devices | `kernel/src/drivers/mod.rs` | Driver cannot safely map or validate resources |
| P1 | No console/framebuffer lock; IRQ can print during a task print | `console.rs`, `fb.rs`, `sched.rs:182-196` | Interleaved or torn output; reentrancy risk |
| P1 | `static mut` state in MMU/GIC/scheduler has no synchronization | `mmu.rs:72-81`, `gic.rs:56-59`, `sched.rs:39-48` | Data races as soon as SMP starts |
| P1 | No physical allocator or ownership model for the UEFI map | `memmap.rs`, `memory.rs` | Cannot safely hand memory to drivers |
| P2 | ACPI per-entry lengths are not validated before field reads | `madt.rs:49-103`, `dbg2.rs:44-60` | Short entries can read into adjacent entries |
| P2 | ACPI table ranges are not checked against mapped memory | `reader.rs:33-54` | Corrupt length can fault |
| P2 | ~~`boot.S` overwrites `CPACR_EL1` instead of read-modify-write~~ **Fixed** | `boot.S` | Preserves firmware-configured coprocessor bits |
| P2 | ~~`boot.S` does not set `SPSel` explicitly~~ **Fixed** | `boot.S` | EL1h `SPSR`/`SP_EL1` are set on the EL2 drop path; EL1 entry uses `sp` |
| P2 | `vectors.S` restores only `ELR_EL1`/`SPSR_EL1` | `vectors.S:171-176` | Cannot return from EL2 or to lower EL correctly |
| P2 | `vectors.S` does not save `SP_EL0`, `TPIDR_EL0`, `TTBR0`, debug state | `vectors.S` | Future userspace/SMP context switches incomplete |
| P2 | `pci::first_bar` prints BARs but does not map or validate them | `pci.rs:258-293` | Driver cannot use the reported BAR |
| P2 | ELF loader does not reject overlapping `PT_LOAD` segments | `elf.rs:110-150` | Malformed image can overwrite another segment |
| P2 | Framebuffer BITMASK mode draws full white; no channel masks | `fb.rs:8-15`, `fb.rs:297-302` | Wrong colors on bitmask GOP modes |
| P2 | PL011 clock is hard-coded to 24 MHz | `console.rs:34-35` | Wrong baud on other PL011 platforms |
| P2 | Timer uses `CNTVCT_EL0` for time but `CNTP_*_EL0` for interrupts | `timer.rs:41-48`, `arch/aarch64/mod.rs:164-168` | Inconsistent if `CNTVOFF_EL2 != 0` |
| P2 | `FLAG_QEMU_VIRT` gates PSCI power-off | `main.rs:225-232` | A non-QEMU platform with PSCI cannot power off |
| P2 | Missing ACPI is fatal | `main.rs:210-213` | A platform with no ACPI cannot boot past inventory |

## 9. Build and test results

### 9.1 Toolchain and environment

| Item | Value |
| --- | --- |
| Host | Windows, PowerShell |
| `rustc` | 1.99.1 (58cf8f9c3 2026-10-13) |
| `cargo` | 1.99.1 (5f94df478 2026-08-27) |
| QEMU | 10.1.94 (v10.2.0-rc4) |
| Kernel target | `aarch64-unknown-none` |
| Loader target | `aarch64-unknown-uefi` |

### 9.2 Clean build

Command:

```powershell
cargo clean
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\build.ps1
```

Result: both crates compiled with no warnings. Artifacts:

| Artifact | Size |
| --- | --- |
| `target/aarch64-unknown-none/release/sketchos-kernel` | 673,064 bytes |
| `target/aarch64-unknown-uefi/release/sketchos-uefi.efi` | 46,592 bytes |

ELF header of the kernel: `ET_DYN` (3), `EM_AARCH64` (183), entry `0x0`, 8 program
headers, `p_align = 0x1000`, 26 `R_AARCH64_RELATIVE` relocations.

### 9.3 Unit tests

Command:

```powershell
cargo test -p sketchos-driver-api
```

Result: 3 passed, 0 failed. The doctest is ignored by design. PowerShell reports a
non-zero process exit because Cargo writes progress to stderr; the test summary is
the authoritative result.

### 9.4 QEMU smoke test, normal mode

Command:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -TimeoutSeconds 180
```

Result: **PASS**.

Key lines:

```
Kernel allocation: 694 pages at 0x00000000bc341000 (relocatable, bias 0x00000000bc341000)
Kernel image: ET_DYN, 26 relocations
Kernel entry: 0x00000000bc341000
Entry exception level: EL1, running at EL1
MMU: enabled, 4K granule, identity map, ...
GIC: GICv3+ distributor 0x0000000008000000, redistributor 0x00000000080a0000, timer PPI 30
Timer: CNTP 100 Hz, period 10000000 ticks at 1000000000 Hz
Scheduler: demo complete, 200 ticks, 39 context switches
SKETCHOS 0.0.2 KERNEL OK
QEMU: PSCI SYSTEM_OFF
```

Inventory time in the verification runs: **47,689 us** and **50,058 us**. The audit
run value was 32,905 us; the spread is host load and synchronous console work, not a
kernel regression claim.

### 9.5 QEMU smoke test, framebuffer mode

Command:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -Framebuffer -TimeoutSeconds 180
```

Result: **PASS**. The framebuffer console activated, PCIe enumeration completed, the
scheduler completed 200 ticks / 39 switches, and QEMU powered off through PSCI.
Inventory time in the verification runs: **121,857 us** and **259,901 us**; the
framebuffer console dominates this number and it varies strongly with host load.

### 9.6 QEMU smoke test, relocation mode

Command:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -OccupyLegacy -TimeoutSeconds 180
```

The `-OccupyLegacy` switch creates `\EFI\SketchOS\occupy-legacy` on the ESP. The
loader sees the marker and reserves the old fixed address before loading the kernel.

Result: **PASS**.

```
Relocation test: occupied legacy kernel address 0x0000000040200000 at 0x40200000
Kernel allocation: 694 pages at 0x00000000bc341000 (relocatable, bias 0x00000000bc341000)
Kernel entry: 0x00000000bc341000
SKETCHOS 0.0.2 KERNEL OK
QEMU: PSCI SYSTEM_OFF
```

The kernel boots at a different base, so the loader no longer depends on
`0x40200000` being free.

### 9.7 QEMU smoke test, EL2 entry mode

Command:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -Virtualization -ExpectEl2 -TimeoutSeconds 180
```

`-Virtualization` adds `virtualization=on` to the QEMU machine so EDK2 enters the
kernel at EL2. `-ExpectEl2` makes the smoke test fail unless the kernel reports it.

Result: **PASS**.

```
Entry exception level: EL2, running at EL1
PSCI: compliant yes, conduit SMC
Scheduler: demo complete, 200 ticks, 39 context switches
SKETCHOS 0.0.2 KERNEL OK
QEMU: PSCI SYSTEM_OFF
```

### 9.8 Notes on existing documentation

`docs/performance.md` records the current normal and framebuffer inventory times and
keeps the older audit values as historical context. The spread is host load and
console work, not a regression claim.

`docs/hardware.md` is a useful historical matrix. The maintained matrix required by
the mission is now `docs/hardware-compatibility.md`.

## 10. Architectural decisions that will hinder future compatibility

1. **~~Fixed physical link address.~~ Resolved.** The kernel is a relocatable
   `ET_DYN` image and the loader chooses a free base below 4 GiB.
2. **Identity map as the only address space.** It is fine for early bring-up, but it
   prevents guard pages, W^X, user/kernel separation, and device isolation. A higher
   half or split address space should be designed before the heap and drivers.
3. **~~EL1-only assumption.~~ Resolved for EL2 entry.** `boot.S` now drops EL2 to EL1
   before `kmain`. EL3 remains unsupported.
4. **QEMU bit as a capability switch.** `FLAG_QEMU_VIRT` should select diagnostics or
   a platform profile, not whether PSCI power-off is allowed.
5. **Static driver inventory.** Discovery must produce `Device` values from PCI
   configuration space and the ACPI namespace; a hand-written list cannot scale.
6. **No DMA/IOMMU model.** The laptop has IORT and Qualcomm IOMMUs. Drivers cannot be
   written safely until DMA address translation and ownership exist.
7. **No per-core data or locking.** Adding SMP after drivers will force a rewrite of
   the MMU, GIC, scheduler, console, and allocator state.
8. **No explicit platform profiles.** QEMU and the OmniBook X need separate profile
   data (GIC version/DS, UART kind, timer, PCIe windows, power methods) behind a
   stable interface.

## 11. Prioritized implementation plan

### P0 — Boot blockers and correctness

| # | Work | Why | Acceptance |
| --- | --- | --- | --- |
| 1 | ~~Implement EL2-to-EL1 transition~~ **Done** | Laptop may hand off at EL2 | QEMU `virtualization=on` test reports `Entry exception level: EL2, running at EL1`; vectors, MMU, GIC, timer, scheduler work |
| 2 | ~~Make the kernel load address safe~~ **Done** | Fixed `0x40200000` could fail | Kernel is `ET_DYN`; loader chooses a free base; `-OccupyLegacy` boots with `0x40200000` reserved |
| 3 | Validate ACPI tables before parsing | Bad checksum/length must not be used | Skip bad tables; validate per-entry minimum lengths; boot with a corrupted optional table without interpreting it |
| 4 | Fix GICD_CTLR DS/security-state handling | Laptop is GICv4 and may be DS=0 | Read DS; enable the correct non-secure Group 1 bit; test on QEMU and a DS=0 model if available |
| 5 | Validate timer INTID range | Avoid underflow and wrong PPI | Reject INTIDs outside 16..=31 for PPI; support SPI timer descriptions explicitly |
| 6 | Do not truncate the final memory map | Future allocator safety | Retry `ExitBootServices` with a larger buffer or fail loudly; never silently drop descriptors |
| 7 | Add a console lock and early laptop output path | Reliability and debuggability | No interleaved output under IRQ; early boot messages visible on GOP before MMU |

### P1 — Kernel foundation

| # | Work | Why | Acceptance |
| --- | --- | --- | --- |
| 8 | Physical page allocator from the UEFI map | Needed by every driver and by the heap | Allocate/free pages, honor reserved/loader/ACPI regions, accounting, stress test |
| 9 | Kernel heap and slab allocator | Drivers and kernel services need dynamic memory | `alloc` works in kernel; leak/overflow tests |
| 10 | VMM API: map/unmap/protect, W^X, guard pages | Isolation and correctness | Unmapped null page faults; stack guard faults; code not writable; data not executable |
| 11 | Page-fault handler and TLB maintenance | Recoverable faults and SMP correctness | Fault diagnostics; `tlbi` helpers; shootdown interface |
| 12 | SMP: PSCI `CPU_ON`, per-core stacks/data, per-core GIC/timer | Laptop has 8 cores | All enabled cores come online in QEMU `-smp 4`; per-core timer ticks; IPI test |
| 13 | Interrupt registration and dispatch | Drivers need IRQs | Register/unregister handlers; trigger/polarity; EOI; deferred work |
| 14 | Driver framework from real discovery | Scale to laptop hardware | PCI functions become `Device`; ACPI namespace devices become `Device`; BARs mapped; bind/probe/start/stop |
| 15 | DMA and IOMMU foundation | Safe device I/O | DMA allocation, address translation, SMMUv3 discovery/init, isolation policy |
| 16 | Platform profiles | Separate QEMU, laptop, generic | `PlatformProfile` data for GIC, UART, timer, PCIe, power; no QEMU assumptions in generic code |
| 17 | Monotonic timekeeping | Scheduling and drivers | `CNTPCT`-based clock, wall-clock offset, per-CPU timers, no drift |

### P2 — Laptop hardware bring-up

| # | Work | Notes |
| --- | --- | --- |
| 18 | Qualcomm SPCR UART driver (`0x13`, `0x894000`) | Only from documented register layout; otherwise keep framebuffer as primary |
| 19 | PCIe resource management | Map BARs on demand, assign windows, MSI-X/ITS |
| 20 | NVMe (`144D:A80D`) | Queue pairs, DMA, MSI-X, namespace scan |
| 21 | XHCI/USB (`QCOM0D08`, `QCOM0C8B`, `QCOM0C8C`) | Controller init, port enumeration, HID boot protocol |
| 22 | Wi-Fi (`17CB:1103`) and Bluetooth (`QCOM0C6B`) | Firmware loading, DMA, power sequencing |
| 23 | Display panel and GPU (`QCOM0D17`) | DSI/panel init, Adreno firmware, IOMMU, power domains |
| 24 | Input (`ELAN010E`, `QTEC0001`) | I2C HID, ACPI namespace/AML, interrupt routing |
| 25 | Audio (`QCOM0C1B`) | DSP firmware, clocks, power domains |
| 26 | Battery/thermal and TPM2 | ACPI AML/EC, thermal zones, TPM command interface |

### P3 — Performance and productization

| # | Work | Notes |
| --- | --- | --- |
| 27 | Framebuffer console rewrite | Glyph atlas, row buffering, dirty rectangles, no per-pixel atomics, optional double buffer |
| 28 | MMU mapping strategy | Map only needed regions; lazy ECAM; larger blocks where valid; reduce L3 pressure |
| 29 | Scheduler rewrite | Idle task with `WFI`, priority, affinity, sleep/wake, per-core queues |
| 30 | Locking and per-core data | Ticket locks, per-core caches, measure contention |
| 31 | Measurement suite | PMU cycle counters, boot-phase timestamps, scheduler/interrupt latency, p50/p99/max |
| 32 | Userspace | EL0, syscalls, address spaces, init, filesystem |

## 12. Recommended immediate next steps

1. **Do not start with drivers.** Continue the P0 set: ACPI validation, GIC DS
   handling, and memory-map integrity. The EL2 drop and relocatable load address are
   complete and QEMU-verified.
2. **~~Add a QEMU EL2 test path.~~ Done.** `-Virtualization -ExpectEl2` enters at EL2
   and asserts the drop to EL1.
3. **~~Add a QEMU test that deliberately occupies `0x40200000`.~~ Done.** The
   `-OccupyLegacy` switch reserves the old address and the kernel still boots.
4. **Add a memory-map fault-injection test.** Corrupt optional ACPI table checksums
   and verify the kernel skips them.
5. **Before any physical laptop test**, complete the remaining P0 set and obtain
   explicit approval, a removable USB drive letter, and a Secure Boot decision. Do not
   modify Windows boot configuration, firmware settings, or the internal disk.
6. **Keep `docs/hardware-compatibility.md` as the single maintained matrix** and
   `docs/hardware.md` as historical context.

## 13. Appendix: QEMU evidence excerpts

### 13.1 Normal mode (excerpt)

```
SketchOS UEFI loader 0.0.2 (AArch64)
ACPI RSDP: ... rev 2 OEM "BOCHS " XSDT ...
ACPI tables: ... entries, checksum ok
Kernel allocation: 694 pages at 0x00000000bc341000 (relocatable, bias 0x00000000bc341000)
Kernel image: ET_DYN, 26 relocations
Kernel entry: 0x00000000bc341000
SketchOS kernel 0.0.2 (AArch64)
Exceptions: VBAR_EL1 0x..., table aligned: yes
Entry exception level: EL1, running at EL1
MMU: enabled, 4K granule, identity map, ...
PCIe: 2 functions on 1 buses across 1 segments
GIC: GICv3+ distributor 0x0000000008000000, redistributor 0x00000000080a0000, timer PPI 30
Timer: CNTP 100 Hz, period 10000000 ticks at 1000000000 Hz
Scheduler: 4 tasks, 16 KiB stacks, round-robin, 5-tick timeslice, 200-tick demo
Scheduler: demo complete, 200 ticks, 39 context switches
SKETCHOS 0.0.2 KERNEL OK
QEMU: PSCI SYSTEM_OFF
```

### 13.2 Framebuffer mode (excerpt)

```
Framebuffer console: active
Framebuffer: 800x600, stride 800, bgr @ 0x..., 3 MiB
...
Kernel inventory time: 121857 us
Scheduler: demo complete, 200 ticks, 39 context switches
SKETCHOS 0.0.2 KERNEL OK
QEMU: PSCI SYSTEM_OFF
```

### 13.3 Relocation and EL2 mode (excerpts)

```
Relocation test: occupied legacy kernel address 0x0000000040200000 at 0x40200000
Kernel allocation: 694 pages at 0x00000000bc341000 (relocatable, bias 0x00000000bc341000)
Kernel entry: 0x00000000bc341000
SKETCHOS 0.0.2 KERNEL OK
QEMU: PSCI SYSTEM_OFF
```

```
Entry exception level: EL2, running at EL1
PSCI: compliant yes, conduit SMC
SKETCHOS 0.0.2 KERNEL OK
QEMU: PSCI SYSTEM_OFF
```

### 13.4 Commands used

```powershell
cargo clean
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\build.ps1
cargo test -p sketchos-driver-api
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -TimeoutSeconds 180
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -Framebuffer -TimeoutSeconds 180
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -OccupyLegacy -TimeoutSeconds 180
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -Virtualization -ExpectEl2 -TimeoutSeconds 180
```

## 14. Appendix: audit limitations

- No physical laptop boot was attempted. All laptop statements are firmware facts
  from ACPI/PCI identifiers, not test results.
- No GICv4, DS=0, SMP, or fragmented-memory-map scenario was executed. The
  corresponding findings are code-review findings, clearly labeled. EL2 entry is now
  executed in QEMU.
- QEMU timing numbers are not performance claims. They are diagnostics from one host
  run and vary with host load.
- The repository is not a Git repository, so there is no commit hash, diff, or review
  history to reference.
