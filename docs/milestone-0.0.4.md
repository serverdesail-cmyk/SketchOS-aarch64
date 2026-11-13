# Milestone 0.0.4 record

**Status:** complete and verified under QEMU on 2026-11-11.

This record covers what the 0.0.4 kernel actually does, how it was verified, and
what it deliberately does not do. Every claim below is backed by a passing
`tools/smoke-test.ps1` run in the mode named next to it. It supersedes the
0.0.3 record but does not claim any capability that was not observed in the
QEMU serial log.

## Scope

Milestone 0.0.4 moves the kernel out of the firmware identity map and gives it
its own virtual memory. It keeps all 0.0.3 behaviour and adds:

1. A position-independent `ET_DYN` kernel image, relocated by the loader to a
   free physical range below 4 GiB.
2. A boot ABI version 2 record that describes the bootstrap identity root, the
   higher-half root, `TCR_EL1`/`MAIR_EL1`, the kernel virtual range and the
   linear physical map.
3. A kernel-owned final stage-1 map with W^X section permissions, a 512 GiB
   linear physical map, Device MMIO, Normal RAM and a Normal non-cacheable
   framebuffer.
4. A W^X audit that walks every leaf descriptor before the success marker.
5. Recoverable synchronous faults: kernel read/write/execute probes and a
   lower-EL fault hook can resume without parking the core.
6. `AddressSpace` with per-space TTBR0 roots, ASID allocation, `activate`,
   `restore`, `map`, `unmap` and `protect`, exercised on the real MMU.
7. Low-half sealing (`TCR_EL1.EPD0`) after every secondary is online.
8. Version strings, crate versions and documentation updated to 0.0.4.

The idle path, driver binding, user processes and system calls were explicitly
out of scope. 0.0.5 is planned on top of this foundation.

## Verified QEMU configuration

All results in this record come from:

```text
QEMU 10.1.94 (v10.2.0-rc4) on Windows
-machine virt,gic-version=3 -cpu max -smp 4 -m 2048
EDK2 AArch64 firmware (edk2-stable202408-prebuilt)
release build of the 0.0.4 tree
```

The four verification modes were run against the same build:

| Mode | Command flag | Entry | PSCI conduit | Result |
| --- | --- | --- | --- | --- |
| Normal | none | EL1 | HVC | PASS |
| Framebuffer | `-Framebuffer` | EL1 | HVC | PASS |
| Relocation | `-OccupyLegacy` | EL1 | HVC | PASS |
| EL2 entry | `-Virtualization -ExpectEl2` | EL2, then EL1 | SMC | PASS |

Each mode ended with `SKETCHOS 0.0.4 KERNEL OK`, printed
`QEMU: requesting PSCI SYSTEM_OFF`, and QEMU exited before the 120-second
timeout. The smoke test requires the loader and kernel banners, the higher-half
geometry, a clean W^X audit, both MMU self-tests, the sealed low half, the
BootInfo v2 layout, PCIe enumeration, all four cores online, every secondary
answering an IPI, every core finishing the 200-tick demo, all eight task FP
states matching their iteration counts, concurrency to be verified, and the
success marker.

The captured logs are `build/evidence-normal.log`,
`build/evidence-framebuffer.log`, `build/evidence-occupy.log` and
`build/evidence-virtualization.log`. They are generated files, not source.

## Boot ABI version 2

[`docs/boot-abi.md`](boot-abi.md) is the full specification. The parts that
matter for this milestone:

- The record is still passed in `x0`, but every pointer in it is a **physical**
  address. The kernel reaches those addresses only through the physical map.
- The record grows from 208 to **272 bytes** and appends `ttbr0_boot_root`,
  `ttbr1_root`, `tcr`, `mair`, `kernel_va_start`, `kernel_va_end`,
  `physmap_base` and `physmap_size`.
- `FLAG_HIGHER_HALF` (bit 6) is required. A version-1 or higher-half-less record
  is rejected because the entry code cannot switch regimes safely.
- The loader builds two roots. TTBR0 identity-maps only the kernel image and the
  boot record for `boot.S`; TTBR1 maps the kernel at
  `KERNEL_VA_BASE + physical` and the whole physical range at
  `PHYSMAP_BASE + physical`.
- `ET_DYN` relocations write the target at the **physical** address and the
  relocated value at the **higher-half** address. Addends are bounded by the
  page-rounded image span so one-past-the-end section symbols stay valid.
- The kernel validates alignment, magic, version, size, both roots, the
  physmap base and size, the kernel physical and virtual ranges, and both walks
  in `TCR_EL1` before dereferencing anything.

Observed in the normal run:

```text
Boot info: 0x00000000bcb40000, version 2, 272 bytes, flags 0x0000007b
Kernel allocation: 2722 pages at 0x00000000bbad4000 (relocatable, phys bias 0x00000000bbad4000, virt bias 0xffff8000bbad4000)
Kernel: 1203768 bytes, 5 segments, 10888 KiB physical at 0x00000000bbad4000..0x00000000bc576000
Kernel image: ET_DYN, 43 relocations
Kernel entry: physical 0x00000000bbad4000, virtual 0xffff8000bbad4000
```

`flags = 0x7b` sets bits 0, 1, 3, 4, 5 and 6: memory map, RSDP, UART,
firmware identity map, QEMU and higher half. Bit 2 is clear in the normal log
because `-display none` gives the loader no linear GOP framebuffer.

## Higher half and physical map

The virtual split is 48-bit, 4 KiB granule, four levels, with
`TCR_EL1.T0SZ = T1SZ = 16`:

| Range | Owner | Contents |
| --- | --- | --- |
| `0x0000_0000_0000_0000` – `0x0000_FFFF_FFFF_FFFF` | TTBR0 | User address spaces; empty in the kernel context after sealing |
| `0xFFFF_8000_0000_0000` – `0xFFFF_BFFF_FFFF_FFFF` | TTBR1 | Kernel image, `KERNEL_VA_BASE + physical` |
| `0xFFFF_C000_0000_0000` – `0xFFFF_FFFF_FFFF_FFFF` | TTBR1 | Linear physical map, `PHYSMAP_BASE + physical` |

`mmu::init` rebuilds TTBR1 from linker section boundaries. The final policy is:

| Region | Permissions | Attributes |
| --- | --- | --- |
| `.text` | read-only, executable at EL1 | Normal write-back |
| `.rodata` | read-only, non-executable | Normal write-back |
| `.data`, `.bss`, early stack, scheduler and secondary stacks | read/write, non-executable | Normal write-back |
| Physmap, default | read/write, non-executable | Device-nGnRnE |
| Physmap, RAM-like descriptors | read/write, non-executable | Normal write-back |
| Physmap, UART | read/write, non-executable | Device-nGnRnE |
| Physmap, framebuffer | read/write, non-executable | Normal non-cacheable |
| Task-stack guard pages | unmapped | — |

The page tables come from a fixed 8 MiB BSS pool of 2048 tables. QEMU needs 23
tables in framebuffer mode, 24 in normal mode and 25 in the relocation mode
(the extra descriptor changes the physmap shape). If the pool is exhausted,
`mmu::init` fails and the kernel keeps running under the loader map instead of
corrupting memory.

Observed in the normal run:

```text
MMU: higher half, TTBR1 0x00000000bbd65000, TTBR0 boot 0x00000000bb2d4000, 24 tables of 2048, physmap 3 GiB
MMU: W^X audit clean (no writable-executable leaf)
Kernel: physical 0x00000000bbad4000-0x00000000bc576000 (10888 KiB), virtual 0xffff8000bbad4000-0xffff8000bc576000
  running at 0xffff8000bbade918, inside the higher-half image: yes
  higher half: physmap 0xffffc00000000000-0xffffc000c0000000 (3 GiB), boot root 0x00000000bb2d4000, kernel root 0x00000000bb2da000
  TCR 0x00000005b5103510, MAIR 0x00000000004400ff
```

The W^X audit walks every leaf and fails the boot if a leaf is both writable and
executable. `boot.S` clears `SCTLR_EL1.WXN`, so permissions come from the
per-page `AP[2]`/`PXN`/`UXN` bits rather than a global write-implies-NX policy.

## Recoverable faults

The exception handler treats a synchronous fault as recoverable in exactly two
cases:

- a kernel probe marked its resume point before the access, or
- the fault came from a lower EL and a registered `UserFaultHook` returns a
  decision.

`probe_read`, `probe_write` and `probe_execute` record a resume label and an
active flag. A translation or permission abort rewrites `ELR` to that label and
the probe returns `Err`. `probe_execute` also restores the caller's `x30` on
both paths, because a faulting `BLR` may already have overwritten it.

The lower-EL path builds a `FaultInfo` (vector, `ELR`, `SPSR`, `ESR`, `FAR`,
`from_user`) and calls the registered hook. The hook can `Retry`, `Skip`
(`ELR + 4`) or `Terminate`. **0.0.4 installs no hook**; the routing point and
the `FaultAction` enum exist for 0.0.5. A kernel fault never reaches the hook.

The boot self-test deliberately makes three faults on the installed map:

```text
MMU: fault self-test passed (unmapped read, read-only write, NX execute)
```

It reads an unmapped low address, writes to read-only `.text`, and attempts to
execute writable `.bss`. All three must return `Err`; any `Ok` fails the boot.
This is structural evidence that the handler can resume at the probe label and
that the W^X permissions are enforced, not merely advertised. The probe state
is global and the probes are single-core bring-up tools; the self-test runs
before any secondary starts.

## Address spaces and ASIDs

`AddressSpace` is the 0.0.4 foundation for user processes:

- `new_user` allocates an empty root from the same table pool and a unique
  ASID. ASID 0 is reserved for the bootstrap identity map.
- ASIDs come from a monotonic atomic counter masked to the width reported by
  `ID_AA64MMFR0_EL1.ASIDBits` (8 by default, 16 when supported). The rollover
  skips zero.
- `map` accepts only user permissions inside the low half. `unmap` and
  `protect` exist for the 0.0.5 loader.
- `activate` installs the root and ASID in TTBR0, flushes the local TLB and
  returns the previous `TTBR0_EL1` value; `restore` puts it back.
- `destroy` releases the logical identity. Tables stay in the fixed pool;
  there is no reclamation in 0.0.4.

The boot self-test allocates one physical page, maps it read/write into a fresh
user space, activates that space, writes and reads `0x5a17_c0de_dead_beef`
through the user virtual address, restores the bootstrap root and frees the
page. It prints:

```text
MMU: address-space self-test passed (ASID switch and user page)
```

## Low-half sealing and SMP

A secondary starts with the MMU off at a physical address with a physical
stack. Its stub installs both roots, biases the stack, `TPIDR_EL1`, the record
and the Rust entry into the higher half, enables the MMU and branches to Rust.
Once every expected secondary has reported online,
`smp::start_secondaries` calls `mmu::seal_low_half` on the boot core, which
sets `TCR_EL1.EPD0`. TTBR0 walks are then disabled and any stray physical
dereference faults instead of silently aliasing kernel memory.

The full SMP demo still runs: four cores online, one ping/pong IPI per
secondary, two guarded tasks per core, 200 ticks and 39 context switches each,
all eight FP states `fp ok`, and `SMP: concurrency verified`.

The PSCI conduit is read from the FADT ARM boot flags, never guessed. It
depends on where firmware started: without `virtualization=on`, EDK2 runs at
EL1 and selects `HVC`; with `virtualization=on`, EDK2 starts at EL2 and selects
`SMC`. Our kernel enters at EL2 in that mode and drops to EL1.

## Corrections made during this milestone

1. **Swapped `AP` bits.** `AP_RO` and `AP_USER` were transposed. QEMU's
   `get_S1prot` then treated read-only pages as writable by EL0 and forced
   execute-never for EL1 fetches, producing a first-fetch permission fault.
   Both constants and the audit now use `AP[2] = 1 << 7` (read-only) and
   `AP[1] = 1 << 6` (EL0 access).
2. **Unaligned map span.** `mmu::init` passed an unrounded memory-map length to
   `map_range`, which requires page alignment. The boot failed with
   `MMU: unavailable: failed to map the memory-map array`. The span is now
   rounded up to a page.
3. **Stacks in read-only memory.** Immutable `static` arrays land in `.rodata`,
   which the final map makes read-only, so the first trap-frame write faulted.
   `sched::STACKS` and `smp::SECONDARY_STACKS` are now `UnsafeCell` wrappers
   (`StackSet`) that land in `.bss`.
4. **Fault probes.** The probe and lower-EL hook API was written for 0.0.5. A
   boot self-test now exercises all three probe directions, and
   `probe_execute` restores `x30` on the fault path.
5. **Versioning.** `sketchos-bootinfo`, `sketchos-uefi`, `sketchos-kernel` and
   `sketchos-mm` are 0.0.4. `sketchos-driver-api` keeps its independent 0.1.0
   API version.
6. **Temporary bring-up probes removed.** The extra prints in `boot.S`, the
   UEFI handoff and `kernel/src/main.rs` are gone, as are `tools/qmon.ps1` and
   `build/symbolize.py`.
7. **Warnings.** The unused unsafe blocks, self-assignment and unused
   constants/accessors were removed or annotated; a clean build is warning-free.

## Verification commands

From the repository root:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\build.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -TimeoutSeconds 120
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -Framebuffer -TimeoutSeconds 120
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -OccupyLegacy -TimeoutSeconds 120
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -Virtualization -ExpectEl2 -TimeoutSeconds 120
cargo test -p sketchos-mm
cargo test -p sketchos-driver-api
```

Host test results:

```text
cargo test -p sketchos-mm           -> 31 passed, 0 failed
cargo test -p sketchos-driver-api   ->  3 passed, 0 failed (1 ignored)
```

The page-table tests cover descriptor round-trips, block splitting, `map_range`
alignment, the `AP[2]`/`AP[1]` bit positions, W^X across all four permission
classes, and the 48-bit TCR/MAIR values.

QEMU results, all four modes:

```text
normal          PASS  EL1 entry, HVC, 24 tables
framebuffer     PASS  EL1 entry, HVC, 23 tables, framebuffer console active
relocation      PASS  EL1 entry, HVC, 25 tables
virtualization  PASS  EL2 -> EL1, SMC, 24 tables
```

## Known limitations

- No userspace, system calls, process model or ELF loader. `AddressSpace` is
  used only by the boot self-test.
- No user-fault policy: the hook is unset and `FaultAction::Terminate` currently
  falls through to the halt path.
- No demand paging, copy-on-write, swap or TLB shootdown. An address-space
  switch flushes only the local core's TLB.
- Page tables are never reclaimed. A destroyed `AddressSpace` leaks its tables
  from the fixed pool.
- The physical bitmap covers a fixed 32 GiB span and the heap is capped at
  8 MiB; both are milestone bounds, not final designs.
- No idle task. The scheduler demo busy-spins by design.
- No drivers are bound. Storage, GPU, Wi-Fi, Bluetooth, audio, camera, USB,
  input and the panel are unimplemented.
- The physical laptop has not been booted. Its SPCR UART is interface `0x13`,
  not a PL011, so the current kernel cannot print to it. Its eight-core SMP
  path is untested.
- No performance claim is made. All latency and startup numbers above are QEMU
  TCG diagnostics, not hardware measurements.
