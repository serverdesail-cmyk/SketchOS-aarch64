# Milestone 0.0.3 record

**Status:** complete and verified under QEMU on 2026-11-10.

This record covers what the 0.0.3 kernel actually does, how it was verified, and
what it deliberately does not do. Every claim below is backed by a passing
`tools/smoke-test.ps1` run in the mode named next to it. It supersedes the
0.0.2 record but does not claim any capability that was not observed in the
QEMU serial log.

## Scope

Milestone 0.0.3 turns the single-core 0.0.2 kernel into a multicore kernel with
real memory management. Added or completed in this milestone:

1. A host-testable `sketchos-mm` crate containing a bitmap physical page
   allocator and an address-ordered first-fit kernel heap.
2. Kernel wrappers that feed the UEFI memory map and static storage to those
   allocators (`phys.rs`, `heap.rs`), including page scrubbing on free and a
   hard heap-growth cap.
3. Interrupt-safe spinlocks (`sync.rs`) and per-core records (`percpu.rs`).
4. MADT processor discovery, PSCI `CPU_ON` startup of secondary cores, and
   GICv3 SGI IPIs (`smp.rs`, `arch/aarch64/gic.rs`).
5. A per-core preemptive scheduler with statically assigned tasks and guarded
   stacks (`sched.rs`).
6. Boot-time memory self-tests and measurements (`selftest.rs`).
7. Version strings and documentation updated from 0.0.2 to 0.0.3.

The boot ABI stays at version 1; the loader was only re-versioned.

## Verified QEMU configuration

All results in this record come from:

```text
QEMU 10.1.94 (v10.2.0-rc4) on Windows
-machine virt,gic-version=3 -cpu max -smp 4 -m 2048
EDK2 AArch64 firmware (edk2-stable202408-prebuilt)
release build of the 0.0.3 tree
```

The four verification modes were run against the same build:

| Mode | Command flag | Result |
| --- | --- | --- |
| Normal | none | PASS |
| Framebuffer | `-Framebuffer` | PASS |
| Relocation | `-OccupyLegacy` | PASS |
| EL2 entry | `-Virtualization -ExpectEl2` | PASS |

Each mode ended with `SKETCHOS 0.0.3 KERNEL OK`, printed
`QEMU: requesting PSCI SYSTEM_OFF`, and QEMU exited before the 180-second
timeout. The smoke test requires all four cores to report online, every
secondary to answer an IPI, every core to finish the 200-tick demo, all eight
task FP states to match their iteration counts, and concurrency to be verified.

## Physical memory allocator

The algorithm lives in `sketchos-mm/src/physical.rs`; `kernel/src/phys.rs`
supplies the UEFI map, reservations and the lock.

- 4 KiB pages, two bitmaps: `managed` (may be handed out) and `allocated`
  (currently owned). A page is free when managed and not allocated.
- Reserving a range clears its managed bits, so reserved, MMIO, firmware and
  unavailable regions can never be allocated.
- Freeing an unmanaged page reports an invalid free; freeing a managed but
  unallocated page reports a double free. That is why two bitmaps are used.
- The bitmap span is `highest usable end - lowest usable start`, not the highest
  physical address in the map, so MMIO windows far above RAM do not inflate it.
- Only `CONVENTIONAL` memory is managed. Kernel, loader, boot info, memory map,
  RSDP, framebuffer and UART ranges are reserved explicitly. The self-test also
  checks the ends of every non-conventional firmware descriptor.
- Freed pages are zeroed after the free is accepted and before the allocator
  lock is released. The next owner never observes the previous contents.
- The bitmap is a caller-supplied `[u64; WORDS]` in BSS; the kernel uses
  `WORDS = 131_072`, covering a 32 GiB span (2 MiB of BSS for both bitmaps).

Observed in the normal run:

```text
Physical memory: managed 2006 MiB (513754 pages), free 2006 MiB, span 0x0000000040000000-0x00000000bffff000
Physical latency: 1000 alloc/free pairs in 1770 us (1770 ns/pair)
Physical fragmentation: 9 free runs / largest 475126 pages before, 41 / 475126 after alternating free, 9 / 475126 after coalescing
Physical self-test: PASS (alloc, free, alignment, invalid-free, double-free, scrub, reservations)
```

The scrub line is backed by a self-test that fills a page with `0xa5`, frees it,
and reads three offsets back; all three must be zero. Interrupts are masked and
no secondary is running at that point, so nothing can claim the page between the
free and the read-back.

Host tests: `cargo test -p sketchos-mm` reports 19 passed, 0 failed. They cover
allocation, free, alignment, invalid-free, double-free, fragmentation,
coalescing, reserved pages and exhausted storage.

## Kernel heap

The algorithm lives in `sketchos-mm/src/heap.rs`; `kernel/src/heap.rs` owns the
lock and the growth policy.

- Address-ordered first fit over one or more regions. Neighbours coalesce in
  `O(1)` because blocks are linked in physical address order.
- Every block has a header; an eight-byte back-pointer immediately before the
  returned pointer recovers the header for any power-of-two alignment.
- `alloc` returns `None` and increments `oom_count` on exhaustion without
  modifying the heap. `dealloc` validates the region, back-pointer, magic and
  stored user pointer before changing anything.
- The heap starts with a 256 KiB region and grows only when allocation fails.
  The growth request is sized to the failed request itself, with a 256 KiB
  minimum, instead of always adding one fixed chunk.
- Growth is capped at 8 MiB. A request that cannot fit under the cap fails
  before any physical pages are reserved. The cap is re-checked under the heap
  lock after the physical pages are taken.
- Lock order is heap then physical, never the reverse. Growth takes physical
  pages with the heap lock released; a failed `add_region` releases the pages.
- No interrupt handler allocates; the scheduler and IPI paths use preallocated
  per-core state only.

Observed in the normal run:

```text
Heap latency: 1000 64-byte alloc/free pairs in 467 us (467 ns/pair)
Heap: 512 KiB from 2 region additions, 512 KiB free in 1 blocks, largest 512 KiB, waste 0 KiB
Heap counters: 2007 allocations, 2007 frees, 2 out-of-memory events
Heap self-test: PASS (small, large, aligned, repeated, growth, growth cap, invalid-free, double-free)
```

`region_additions` counts `add_region` calls, not disjoint regions after
coalescing. The two 256 KiB additions are physically adjacent and coalesce into
one 512 KiB free block, so `free_blocks` (1) is smaller than `region_additions`
(2) in a healthy heap. The self-test therefore checks `allocated_bytes == 0`,
`free_bytes == total_bytes` and `free_blocks <= region_additions`; it does not
equate free blocks with region additions.

The two out-of-memory events are expected: one allocation deliberately exceeds
the initial region to trigger growth, and one deliberately exceeds the 8 MiB
cap. The cap test also verifies that `total_bytes` does not change.

## SMP discovery and startup

- The kernel reads enabled MADT `GICC` entries only and keeps each entry's real
  MPIDR affinity value; the logical core index is assigned by the kernel.
- The boot core is identified from its own MPIDR, not assumed to be index 0.
- Each secondary's stack top, translation state and context go into its
  `PerCpu` record. The boot core cleans those writes and the kernel image to the
  point of coherency, then calls PSCI `CPU_ON` with the physical address of
  `secondary_entry` and the record address.
- `CPU_ON` uses the 64-bit function ID only when `PSCI_VERSION` reports 0.2 or
  newer; otherwise it uses the 32-bit form. `SYSTEM_OFF` always uses the 32-bit
  `0x84000008`, which is the only form defined for that function.
- The assembly stub installs the stack, `TTBR0_EL1`/`TCR_EL1`/`MAIR_EL1`, enables
  the MMU and `TPIDR_EL1`, then enters Rust.
- A secondary initializes its own GIC redistributor and timer, marks itself
  online, and waits for the scheduler release.
- Startup is bounded: each core has a counter-based timeout; a core that fails
  PSCI, fails GIC setup or never reports is marked and excluded.

Observed in the normal run:

```text
SMP: 4 discovered, 4 expected online, boot MPIDR 0x0000000000000000, PSCI compliant (HVC)
  core 0: boot, MPIDR 0x0000000000000000, online
  core 1: online, MPIDR 0x0000000000000001, startup 303 us, GICR 0x00000000080c0000
  core 2: online, MPIDR 0x0000000000000002, startup 11594 us, GICR 0x00000000080e0000
  core 3: online, MPIDR 0x0000000000000003, startup 424 us, GICR 0x0000000008100000
```

The startup times are QEMU TCG diagnostics and vary enormously between runs
(150 us to 18 ms was observed for the same core across modes). They are not
hardware boot-time measurements.

## IPIs

GICv3 SGIs are sent with `ICC_SGI1R_EL1`: SGI 0 is a ping, SGI 1 is a pong. Each
core initializes its own redistributor and system-register CPU interface. The
boot core pings each online secondary once.

- Only one ping is outstanding at a time. If a secondary does not answer within
  a bounded timeout, the test fails instead of pinging the next core: a pong
  carries no sender identity, so a late reply must never be credited to another
  core.
- The pong handler records the last round-trip counter delta and increments a
  global pong count. The boot core attributes that delta to the core it just
  pinged and stores it in that core's record.
- Per-core `ipi_pings`, `ipi_pongs` and `ipi_latency_ticks` are therefore
  populated for each pinged core.

Observed in the normal run:

```text
  IPI core 1 (MPIDR 0x0000000000000001): 26079700 ticks round trip
  IPI core 2 (MPIDR 0x0000000000000002): 18600 ticks round trip
  IPI core 3 (MPIDR 0x0000000000000003): 8168600 ticks round trip
IPI: 3 secondaries answered a ping/pong round trip
```

The timer counter is 1 GHz, but QEMU TCG schedules vCPUs on host threads, so
these deltas include host preemption and are not interrupt-latency claims.

## Per-core scheduler

- One `CoreSched` per online core; no task migrates between cores. A core is the
  only writer of its own scheduler state.
- Two statically assigned kernel tasks per core, preemptive round-robin with a
  5-tick timeslice and a 200-tick demo. Each task has its own 16 KiB stack and
  saved trap frame.
- Every task stack has a 4 KiB guard page below it. `sched::init` unmaps all
  guard pages and invalidates the local TLB before any secondary starts. An
  overflow therefore faults instead of silently corrupting the neighbouring
  stack. No intentional overflow was performed, so this is a structural
  guarantee rather than an observed fault test.
- When the demo ends, the timer handler does not report or power off inside IRQ
  context. It marks the core not-ready and returns a synthetic trap frame that
  enters `finish_task` on EL1h with IRQs enabled, after `gic::end` has run.
- The boot core waits for every online secondary with a bounded timeout. The
  wait returns success or failure; a timeout can never be printed as verified.
  Concurrency verification requires per-core `work`, `sched_ticks`, and a
  secondary `sched_done` bit.
- `FP_BITS` is a deterministic accumulator that starts at `task_id + 1` and adds
  `1.0` per iteration. Each task's saved value must equal
  `iterations + task_id + 1`, or that value minus one when an interrupt lands
  between the work-counter increment and the FP store. A mismatch prints
  `MISMATCH` and prevents the verified banner.

Observed in the normal run:

```text
Scheduler: 2 tasks per core, 16 KiB stacks with guard pages, round-robin, 5-tick timeslice, 200-tick demo
Scheduler: core 0 demo complete, 200 ticks, 39 context switches
  core 0 task 0: 39555420 iterations, fp bits 0x4182dc8ae8000000, fp ok
  core 0 task 1: 6800087 iterations, fp bits 0x4159f0b640000000, fp ok
...
SMP: 4 of 4 CPUs online, total work 94084057
SMP: concurrency verified (every online core executed scheduler work)
```

Every core reported 200 ticks and 39 context switches; all eight task lines
reported `fp ok`. The iteration counts differ per core because QEMU schedules
vCPUs independently and the boot core also performs the IPI test and reporting.

## Corrections made during this milestone

The first 0.0.3 attempt stopped before SMP. Both blockers and the associated
correctness concerns were fixed and re-verified:

1. **Heap self-test invariant.** `free_blocks == region_additions` was wrong
   because adjacent regions coalesce. The test now checks byte-level fullness
   and only bounds `free_blocks` by `region_additions`.
2. **PSCI `SYSTEM_OFF` function ID.** The 64-bit `0xC4000008` does not exist;
   the kernel now always uses `0x84000008`. The banner was changed to
   `QEMU: requesting PSCI SYSTEM_OFF` and the call result is reported, so the
   banner no longer pretends the request succeeded.
3. **Concurrency evidence.** `TOTAL_WORK` was never incremented. It was removed;
   verification now reads per-core counters and requires the secondary
   `sched_done` bit.
4. **IPI accounting.** Global sent/pong counters were misattributed across
   cores. The test now allows one outstanding ping and stores results in the
   pinged core's record.
5. **IRQ-context finish.** The demo epilogue now runs in a synthetic task
   context, not inside the timer interrupt before `gic::end`.
6. **Guard pages.** Each task stack has an unmapped 4 KiB guard page.
7. **Heap growth.** Growth is request-sized and capped at 8 MiB; a request above
   the cap fails without reserving pages.
8. **Page scrubbing.** Freed physical pages are zeroed under the allocator lock.
9. **Reservation tests.** The physical self-test checks the kernel, boot info,
   memory map, and both ends of every non-conventional descriptor.
10. **Build warnings.** The function-cast warnings are gone and the remaining
    dead-code warnings are explicitly annotated as retained API.

## Verification commands

From the repository root:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\build.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -TimeoutSeconds 180
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -Framebuffer -TimeoutSeconds 180
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -OccupyLegacy -TimeoutSeconds 180
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -Virtualization -ExpectEl2 -TimeoutSeconds 180
cargo test -p sketchos-mm
cargo test -p sketchos-driver-api
```

Host test results: `sketchos-mm` 19 passed / 0 failed; `sketchos-driver-api`
3 passed / 0 failed (one documentation test is ignored).

## Known limitations

- The physical laptop has not been booted. Its SPCR UART is interface `0x13`,
  not a PL011, so the current kernel cannot print to it. See
  `docs/real-hardware.md`.
- Only `CONVENTIONAL` memory is managed; the bitmap covers a fixed 32 GiB span.
  A machine whose usable span exceeds that fails initialization instead of
  silently managing a subset.
- Freed pages are scrubbed, but there is no page-fault recovery, no W^X, and no
  per-page permission enforcement. A guard-page hit is fatal.
- Synchronous exceptions remain fatal; a fault is reported and the core parks.
- No task migration exists. A task is permanently bound to one core.
- No userspace, filesystem, system-call interface or bound drivers.
- No 0.0.3 performance claim is made. All numbers above come from QEMU and are
  diagnostics, not hardware benchmarks.
