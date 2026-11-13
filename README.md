# SketchOS

SketchOS is an independent operating system for 64-bit Arm (AArch64) computers,
written primarily in Rust. It is **not** a Linux distribution and does not use
the Linux kernel. Any Linux compatibility will be an explicit, optional
subsystem under [`compat/linux`](compat/linux/README.md), never the foundation
of the OS.

The primary development target is an HP OmniBook X 14-fe1xxx laptop
(Snapdragon X Plus X1P42100, 8 Oryon cores, 16 GiB RAM, Adreno X1-45 GPU,
2240x1400 display). The boot path is kept generic across UEFI AArch64 machines,
and every change is validated in QEMU before it is considered working.

## Project status

**Milestone 0.0.6 is verified under QEMU** (2026-11-12). The kernel now runs an
EEVDF scheduler with one run queue per core, cluster-aware work stealing driven
by ACPI PPTT, one-shot per-core `CNTP` timers with tickless idle, a debug-only
invariant checker, and per-thread wake-latency/run-delay diagnostics. It keeps
the 0.0.5 EL0 userspace: per-process address spaces, a versioned `svc` ABI, a
static AArch64 ELF loader, processes and threads, a minimal `init`, and bounded
message IPC. It also keeps everything from 0.0.4: UEFI boot, ACPI/PCIe
inventory, GICv3, PSCI startup of all eight QEMU CPUs, SGI IPIs, the
higher-half kernel with a linear physical map, W^X, and guarded kernel stacks.
The build now produces a bootable FAT32 ESP image, and QEMU boots that image
directly as part of verification.

The 0.0.6 tree contains:

- An EEVDF scheduler in the host-testable `sketchos-sched-core` crate: weighted
  virtual time, a requested slice treated as a deadline request, Linux-style
  eligibility, a symmetric lag clamp, and new threads joining at the run queue
  floor.
- A fixed-capacity indexed AVL run tree per core, keyed by
  `(virtual deadline, entity id)`. The running entity is outside the tree and
  still counts toward the floor and the load-weighted average.
- Cluster-aware stealing: same-cluster victims first, the victim's
  latest-deadline runnable entity, and a rate-limited `SGI_BALANCE` push path.
- A one-shot per-core physical timer. The next event is the minimum of the slice
  end, local timer waits, the run-tree head deadline and the demo end; an empty
  queue disarms the timer and the core waits in `WFI`.
- A debug-only invariant checker over the run trees, entity states, duplicate
  detection, affinity and single-core ownership.
- Per-thread wake-latency and run-delay count/sum/max/log2 histogram, reported
  with p50/p99 bucket upper bounds and labelled as QEMU-TCG diagnostics.
- An additive `SYS_SLEEP = 6` and a latency workload with a short-slice sleeper
  and a long-slice CPU hog.
- A bootable FAT32 EFI System Partition image built without touching a physical
  disk (`tools/make-esp-image.ps1`).
- EL0 execution with one user address space per process, TTBR0/ASID switching,
  W^X user pages, per-thread 16 KiB user stacks, and a 16 KiB guarded kernel
  stack per thread.
- A versioned `svc #0` ABI: call number in `x8`, arguments in `x0..x3`, result in
  `x0`, negative errno values on failure. Calls 0-6 are `abi_version`, `exit`,
  `write`, `yield`, `send`, `recv` and `sleep`.
- The 0.0.5 ELF parser, process/thread model, six embedded test images, fault
  routing, bounded IPC and memory management, all preserved.
- Everything from 0.0.4: the relocatable higher-half kernel, Boot ABI v2, W^X
  audit, recoverable kernel faults, `AddressSpace`, low-half sealing, host-tested
  allocators, SMP/IPIs and guarded-stack scheduling.

Every mode is verified by the same binary on eight QEMU vCPUs: normal,
two-cluster (`-TwoCluster`), framebuffer (`-Framebuffer`), relocation
(`-OccupyLegacy`), EL2 entry (`-Virtualization -ExpectEl2`) and the standalone
ESP image (`-EspImage`). A debug build (`-Debug`) additionally runs the
invariant checker. Each run ended with `SKETCHOS 0.0.6 KERNEL OK` and a clean
PSCI `SYSTEM_OFF` on QEMU.

SketchOS is still an early kernel and is **not** a usable operating system yet:

- The scheduler uses one IRQ-safe lock for the whole state, not per-core locks.
  The per-core queues and stealing are real; the locking is coarser than the
  design notes called for and is not a scalability claim.
- User threads are pinned to the boot core. There is no user migration, no
  cross-core user wakeup and no TLB shootdown. The migration workload is the
  kernel demo.
- No shared-memory IPC, no IPC timeout or queue, and no signal/cancellation.
- No `fork`, `exec`, `wait`, filesystem, shell, or way to load another image.
  The six test images are embedded at build time.
- No page-table reclamation: an exited process frees its user pages, but its
  tables stay in the fixed kernel pool.
- No general-purpose device drivers are bound. The shell can read input only
  from a firmware-described PL011 UART. USB HID keyboard, the laptop's
  Qualcomm UART, GPU, Wi-Fi, Bluetooth, audio, camera and native panel drivers
  remain unimplemented.
- The physical laptop has not been booted. Its SPCR UART is interface `0x13`,
  not a PL011, and its eight-core SMP path is untested.
- No performance claims. QEMU cannot model Snapdragon or Adreno performance.
  All scheduler and latency numbers are QEMU-TCG diagnostics.

The verified 0.0.6 claims are deliberately narrow:

- A UEFI loader loads a **separate**, relocatable ELF64 AArch64 kernel from
  `\EFI\SketchOS\kernel.elf`, applies `R_AARCH64_RELATIVE` relocations, and
  enters it with a version-2 boot record.
- `ExitBootServices` is called correctly, and no UEFI Boot Service is used
  afterwards.
- The loader builds and describes two translation regimes: a bootstrap TTBR0
  identity map for entry, and a TTBR1 map containing the higher-half kernel image
  plus a linear physical map.
- The kernel installs its own TTBR1 map with `.text` RX, `.rodata` R, everything
  writable NX, Device MMIO, Normal RAM and a Normal non-cacheable framebuffer.
- A W^X audit of every leaf descriptor passes before the success marker.
- Every physical byte under 512 GiB is reachable at `PHYSMAP_BASE + physical`;
  no code dereferences a physical address directly.
- Three boot fault self-tests recover: an unmapped read, a write to read-only
  `.text` and an instruction fetch from writable `.bss`.
- Per-address-space TTBR0 roots and ASIDs work on the real MMU; a user page is
  written and read through a mapped address space.
- The low half is sealed after SMP startup and unsealed only for the boot-core
  user phase.
- The physical allocator and heap keep their 0.0.3/0.0.4 behaviour, including
  reservations, scrubbing, double-free detection and the 8 MiB growth cap.
- All eight QEMU CPUs are started through PSCI `CPU_ON`, answer an SGI ping,
  and complete the EEVDF demo.
- ACPI PPTT is decoded into a core-to-cluster map. The normal QEMU topology
  reports 10 nodes / 1 cluster; `-smp 8,clusters=2,cores=4` reports 12 nodes /
  2 clusters with cores 0-3 and 4-7.
- The EEVDF demo starts 11 kernel workers on the boot cluster with equal virtual
  time and deadline, migrates workers with same-cluster-first stealing, and
  reports `7 steals, 7 migrations` in both topology modes. The two-cluster run
  observed `3 same-cluster, 4 cross-cluster`; the flat run observed `7
  same-cluster, 0 cross-cluster`.
- The one-shot timer armed 235-1146 events depending on the run, disarmed 15
  times per run for tickless idle, and never reported a concurrency failure.
- The debug invariant checker reported `293 checks, 0 violations` in the debug
  evidence run; release builds compile it out.
- Five static ELF images load at EL0 in separate address spaces, plus the 0.0.6
  latency workload: `Userspace: 6 processes, 8 threads, 46 pages`.
- `init` queries ABI version `0x0001_0000`, writes its lines, yields and exits
  0. Two processes exchange `ping` and `pong` through bounded IPC. A two-thread
  process prints from both threads.
- The latency sleeper records 25 wake samples and the hog records run delay;
  the report is labelled `QEMU-TCG diagnostics, not performance claims`.
- A deliberate EL0 data abort in pid 4 terminates only pid 4 (`ESR
  0x92000006`, `FAR 0x00001000`); the other five processes finish and all six
  are reaped with zero user pages.
- `tools/make-esp-image.ps1` builds a 65 MiB MBR/FAT32 image with one type-0xEF
  partition, verifies its FAT chains and both files by SHA-256, and QEMU boots
  that image file directly.
- `tools/smoke-test.ps1` asserts the 0.0.6 banners, the 0.0.4/0.0.5 geometry and
  self-tests, the eight-CPU SMP handoff, the EEVDF and timer lines, the steal
  counts, the invariant line in debug builds, the latency workload, the
  six-process summary, the fault, the success marker and a clean power-off.

See [docs/milestone-0.0.6.md](docs/milestone-0.0.6.md) for the full evidence
record and known limitations. [docs/syscalls.md](docs/syscalls.md) specifies the
system-call ABI and user address-space layout.
[docs/boot-abi.md](docs/boot-abi.md) specifies the version-2 handoff,
[docs/memory-management.md](docs/memory-management.md) and
[docs/smp.md](docs/smp.md) describe the memory and SMP designs, and
[docs/performance.md](docs/performance.md) records the QEMU diagnostics and the
measurement policy. Earlier records are preserved in
[docs/milestone-0.0.5.md](docs/milestone-0.0.5.md),
[docs/milestone-0.0.4.md](docs/milestone-0.0.4.md),
[docs/milestone-0.0.3.md](docs/milestone-0.0.3.md),
[docs/milestone-0.0.2.md](docs/milestone-0.0.2.md) and
[docs/milestone-0.0.1.md](docs/milestone-0.0.1.md).

## Requirements

- Windows PowerShell (the helper scripts are PowerShell).
- Rust **1.99.1** via `rustup`, with the targets pinned in
  [`rust-toolchain.toml`](rust-toolchain.toml):
  - `aarch64-unknown-none`
  - `aarch64-unknown-uefi`
- QEMU 10.x for AArch64 at `C:\Program Files\qemu`, including
  `qemu-system-aarch64.exe` and the EDK2 AArch64 firmware
  (`share\edk2-aarch64-code.fd`, `share\edk2-arm-vars.fd`).
- On Windows, the MSVC linker and Windows SDK for the UEFI target.

The project builds with the pinned toolchain; a different Rust version may work
but is not what the release evidence was produced with.

## Quick start

From the repository root:

```powershell
# Build the kernel and loader, assemble build/esp, create the bootable
# build/sketchos-esp.img, and copy images/.
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\build.ps1

# Boot QEMU with a serial console window/log.
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\run-qemu.ps1

# Automated boot validation (uses the dedicated smoke-test build and expects QEMU poweroff).
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -TimeoutSeconds 180

# Two PPTT clusters (QEMU -smp 8,clusters=2,cores=4).
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -TwoCluster -TimeoutSeconds 180

# Debug build with the scheduler invariant checker.
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -Debug -TimeoutSeconds 300

# Boot the standalone FAT32 ESP image instead of build/esp.
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -EspImage .\build\sketchos-esp.img -TimeoutSeconds 180

# Same validation with a framebuffer console (adds QEMU ramfb).
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -Framebuffer -TimeoutSeconds 180

# Diagnostic only: reproduce the WHPX failure (expected, see docs).
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -Accel whpx -TimeoutSeconds 60

# Host tests for the scheduler core, ELF parser, page tables, allocator and
# driver API. The kernel binary itself cannot be host-tested (panic_impl).
cargo test -p sketchos-sched-core
cargo test -p sketchos-elf
cargo test -p sketchos-mm
cargo test -p sketchos-driver-api
```

A successful smoke test ends with the user-phase summary and:

```text
Userspace: 6 processes, 8 threads, 46 pages, 16 KiB user stacks
...
SKETCHOS 0.0.6 KERNEL OK
QEMU: requesting PSCI SYSTEM_OFF
PASS: kernel reached the success marker and powered off.
```

The user-phase line order varies with scheduling; the test checks that each
expected line is present, not a fixed total order. Before the user phase the log
still contains the unchanged boot checks:

```text
MMU: W^X audit clean (no writable-executable leaf)
MMU: address-space self-test passed (ASID switch and user page)
MMU: fault self-test passed (unmapped read, read-only write, NX execute)
MMU: TTBR0 walks disabled; low half sealed
SMP: 8 of 8 CPUs online
SMP: concurrency verified (every online core executed scheduler work)
Scheduler: boot core handing off to the user phase
```

The scheduler section adds:

```text
Scheduler: EEVDF, 11 kernel workers, 16 KiB stacks with guard pages, 4000 us slice request, 1000-ms demo
Sched: 8 per-core run queues, one-shot CNTP events, 1 cluster, cluster-aware stealing
Sched: 7 steals, 7 migrations (7 same-cluster, 0 cross-cluster), 0 balance SGIs
Sched: one-shot timer: 1146 events armed, 15 idle disarms (tickless), 1161 IRQ masks
Sched: latency figures are QEMU-TCG diagnostics, not performance claims
```

The event, steal and IRQ-mask counts vary between runs; the smoke test checks
their shape, not a fixed value. `-Debug` replaces the disabled-checker line with
`Sched: invariant checker: N checks, 0 violations`.

Equivalent direct build commands (build the user images first, because the
kernel embeds them with `include_bytes!`):

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\build-userspace.ps1
cargo build --release -p sketchos-kernel --target aarch64-unknown-none
cargo build --release -p sketchos-uefi --target aarch64-unknown-uefi
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\make-esp-image.ps1
```

### QEMU configuration

The smoke test and runner use the same machine definition:

```text
-machine virt,gic-version=3,memory-backend=sketchos-ram
-object memory-backend-ram,id=sketchos-ram,size=2048M,prealloc=on
-overcommit mem-lock=off
-accel tcg,thread=multi,tb-size=256 -cpu max -smp 8 -m 2048
-drive if=pflash,format=raw,readonly=on,file=edk2-aarch64-code.fd
-drive if=pflash,format=raw,file=build/edk2-vars.fd
-drive if=virtio,format=raw,file=fat:rw:build/esp
-nic none -display none -monitor none -no-reboot
-serial file:build/serial.log
```

Guest RAM is one explicit backend. `prealloc=on` keeps host page faults out of
timed runs; `mem-lock=off` keeps the pages reclaimable, which matters on a
unified-memory host where the GPU shares the same DRAM. The smoke test also
raises the QEMU process to High priority and requests 0.5 ms Windows timer
resolution for the run (restoring it afterwards); `-NoTuning` disables those two
host-side steps and `-NoPrealloc` disables preallocation for comparison.

`-TwoCluster` replaces `-smp 8` with `-smp 8,clusters=2,cores=4`, which makes
QEMU's PPTT describe two clusters while the MPIDR values stay 0-7.
`-EspImage` replaces the `fat:rw:build/esp` drive with the standalone image
file, so QEMU boots the artifact that would be written to a USB medium.
`-Accel whpx` selects WHPX and `-cpu host`; it is a documented diagnostic
failure on this host (EDK2 never reaches `BdsDxe`, and WHPX faults on
`CNTP_CVAL_EL0`), so TCG remains the default.

`-display none` means QEMU's GOP has no usable linear framebuffer, so boot-info
flag bit 2 (`FLAG_FRAMEBUFFER`) is intentionally clear in the normal QEMU log.
That is a property of the test configuration, not a loader failure.
`-Framebuffer` adds `-device ramfb` so the loader receives a GOP framebuffer and
the kernel's framebuffer text console is exercised headlessly.

## How the boot works

```text
UEFI firmware
    |
    v
BOOTAA64.EFI  (SketchOS loader, aarch64-unknown-uefi)
    |-- read + validate ACPI RSDP from the UEFI configuration table
    |-- walk XSDT/RSDT, find SPCR and the debug UART
    |-- read \EFI\SketchOS\kernel.elf, validate ELF64 ET_DYN AArch64
    |-- allocate a free physical range, zero it, copy PT_LOAD segments
    |-- apply R_AARCH64_RELATIVE relocations (target physical, value high half)
    |-- clean D-cache, invalidate I-cache
    |-- allocate BootInfo + memory-map array + table pool as EfiLoaderData
    |-- ExitBootServices
    |-- copy the final memory map, patch the boot record
    |-- build TTBR0 bootstrap identity root and TTBR1 higher-half root
    |-- describe TCR/MAIR, kernel VA range and physmap; set FLAG_HIGHER_HALF
    v
kernel.elf  (freestanding kernel, aarch64-unknown-none, physical entry)
    |-- x0 = physical address of BootInfo
    |-- boot.S: clear .bss, install loader roots, EL2 -> EL1, enter high half
    |-- validate the boot record and the memory map through the physmap
    |-- decode ACPI tables, report firmware inventory
    |-- rebuild the final W^X TTBR1 map and audit it
    |-- install exception vectors and recoverable fault routing
    |-- enumerate PCIe through the MCFG ECAM windows
    |-- initialize the physical allocator and run its self-test
    |-- exercise an address space and ASID on the real MMU
    |-- initialize the kernel heap and run its self-test
    |-- discover enabled MADT processors, identify the boot core
    |-- decode PPTT and build the core-to-cluster map
    |-- initialize the GICv3 distributor, boot redistributor and timer
    |-- start every secondary through PSCI CPU_ON
    |-- wait for each secondary, run a ping/pong IPI, seal the low half
    |-- start 11 EEVDF kernel workers across the online cores
    |-- arm one-shot CNTP events, steal work and report per-core results
    |-- hand the boot core to the user phase
    |-- unseal the low half, load six embedded ELF images into separate spaces
    |-- enter EL0; serve svc calls and preempt user threads on the timer
    |-- run init, IPC ping/pong, a deliberate fault, two threads and the
    |   short-slice sleeper / long-slice hog latency workload
    |-- reap every process, report statuses and page counts
    |-- on QEMU, request PSCI SYSTEM_OFF
```

The kernel is built as a relocatable `ET_DYN` image and the loader chooses a
free physical base below 4 GiB, applies the relative relocations and copies the
segments. The `-OccupyLegacy` test deliberately reserves `0x40200000`, the old
fixed address, and proves the kernel boots elsewhere. The full ABI is specified
in [docs/boot-abi.md](docs/boot-abi.md).

## Repository layout

| Path | Purpose |
| --- | --- |
| `boot/protocol/` | `sketchos-bootinfo`, the shared versioned boot ABI crate (`no_std`, dependency-free). |
| `boot/uefi/` | UEFI loader: ACPI discovery, ELF loading/relocation, bootstrap page tables, `ExitBootServices`. |
| `kernel/` | Freestanding AArch64 kernel: entry assembly, MMU/physmap, exceptions, ACPI decoders, PCIe, GIC, timer, physical allocator, heap, SMP, scheduler, processes, syscalls, console. |
| `kernel/linker/kernel.ld` | Kernel linker script (relocatable `ET_DYN`, page-aligned W^X sections). |
| `mm/` | `sketchos-mm`, host-testable `no_std` page-table walker, physical allocator and kernel heap. |
| `elf/` | `sketchos-elf`, host-tested `no_std` parser for static non-PIE AArch64 user images. |
| `sched-core/` | `sketchos-sched-core`, host-tested `no_std` AVL run tree, EEVDF arithmetic, latency histogram and wait queue. |
| `drivers/` | `sketchos-driver-api`, the allocation-free driver vocabulary and lifecycle. |
| `tools/` | Build, QEMU launch, smoke test, ESP-image builder and ACPI inspection scripts. |
| `images/` | Build outputs copied for convenience (`BOOTAA64.EFI`, `sketchos-kernel.elf`, `sketchos-esp.img`). |
| `build/` | Generated ESP, writable EDK2 variables, and serial logs (ignored by Git). |
| `docs/` | Boot ABI, hardware matrix, driver API, memory management, SMP, performance policy, milestone records. |
| `compat/linux/` | Reserved for an optional Linux ABI subsystem. Nothing implemented. |
| `graphics/` | Reserved for the SketchOS graphics stack. Nothing implemented. |
| `userspace/` | User runtime and the embedded EL0 test images (`init`, IPC pair, fault test, two threads, latency workload). |
| `sketchy/` | Reserved for the native Sketchy user-facing environment. Nothing implemented. |
| `libraries/` | Reserved for reusable SketchOS libraries once they have consumers. |
| `tests/` | Reserved for cross-crate integration tests beyond the QEMU smoke test. |
| `config/` | Reserved for image/runtime configuration. Build configuration lives in `.cargo/` and `rust-toolchain.toml`. |

`target/`, `build/`, and generated images are not source. The reset and
firmware files under `C:\SketchOS\hardware` are research inputs outside this
repository and are never modified by the build.

## Hardware support

The 0.0.6 kernel **describes** hardware and programs only the controllers it can
validate under QEMU: the PL011 debug UART, the GICv3 distributor and per-core
redistributors, the per-core generic timer in one-shot mode, PSCI
`CPU_ON`/`SYSTEM_OFF`, the MMU and physical map, the physical page allocator,
the kernel heap, PCIe configuration space, and the ACPI PPTT topology. The
laptop's GPU, NVMe controller, Wi-Fi, Bluetooth, audio DSP, camera ISP, USB
controllers, touchpad, keyboard, and panel are all currently unsupported by
SketchOS.

The laptop's SPCR describes interface type `0x13` at `0x894000`, which is not a
PL011. The kernel therefore intentionally produces no serial output on that
machine yet. Its eight-core SMP path is also unverified: QEMU runs eight vCPUs
in the verification configuration, the laptop has eight enabled Oryon cores,
and no physical boot has been performed.
See [docs/hardware-compatibility.md](docs/hardware-compatibility.md) for the
verified compatibility matrix and the distinction between QEMU-verified behavior,
laptop firmware facts, and unimplemented drivers. See
[docs/memory-management.md](docs/memory-management.md) and
[docs/smp.md](docs/smp.md) for the memory and SMP designs.

## Documentation

- [Boot ABI, version 2](docs/boot-abi.md)
- [System-call ABI and user address space](docs/syscalls.md)
- [Hardware compatibility matrix](docs/hardware-compatibility.md)
- [Memory management and virtual memory](docs/memory-management.md)
- [SMP: discovery, startup, IPIs, scheduling](docs/smp.md)
- [Driver API](docs/driver-api.md)
- [Performance measurement policy](docs/performance.md)
- [Milestone 0.0.6 record](docs/milestone-0.0.6.md)
- [Milestone 0.0.5 record](docs/milestone-0.0.5.md)
- [Milestone 0.0.4 record](docs/milestone-0.0.4.md)
- [Milestone 0.0.3 record](docs/milestone-0.0.3.md)
- [Milestone 0.0.2 record](docs/milestone-0.0.2.md)
- [Milestone 0.0.1 record](docs/milestone-0.0.1.md)
- [Real-hardware bring-up](docs/real-hardware.md)

## Design rules

1. **No pretending.** Unimplemented components are labelled unimplemented, and
   a boot log is evidence only for what it actually printed.
2. **No firmware calls after `ExitBootServices`.** The kernel never calls UEFI
   Boot Services or Runtime Services.
3. **No guessed register layouts.** A device is driven only from a verified
   firmware description or a documented programming interface.
4. **No performance claims without measurements.** See
   [docs/performance.md](docs/performance.md).
5. **Build incrementally.** Every milestone must boot, be reproducible, and
   have an automated or recorded verification step.
6. **Linux is optional.** Linux compatibility is a separate subsystem and does
   not shape the core kernel.

## Next milestone

Milestone 0.0.6 is complete and verified; it is not described again here.
The following remain later work and are deliberately outside 0.0.6:

1. Shared-memory IPC, IPC queues/timeouts, and signals or cancellation.
2. Running user threads on more than the boot core, with user migration and TLB
   shootdown. The current migration workload is the kernel demo.
3. Replacing the single scheduler lock with per-core locks and a genuinely
   concurrent steal path, then measuring scaling rather than assuming it.
4. Process creation after boot: `fork`/`exec`/`wait` and a way to load another
   image instead of embedding it.
5. Reclaiming address-space page tables instead of abandoning them in the fixed
   pool.
6. A first real driver boundary. The laptop's non-PL011 UART also needs a
   documented driver before on-hardware diagnostics are possible.
7. A physical SMP test for the laptop's eight Oryon cores before any laptop
   multicore claim is made. Internal-disk installation remains out of scope
   until the USB boot path is verified on the physical machine.

## License

MIT. See [LICENSE](LICENSE).
