# Milestone 0.0.6 record

**Status:** complete and verified under QEMU on 2026-11-12.

This record covers what the 0.0.6 kernel actually does, how it was verified, and
what it deliberately does not do. Every claim below is backed by a passing
`tools/smoke-test.ps1` run in the mode named next to it, by a host test named at
the end, or by the bootable image that QEMU started directly. Numbers printed by
the kernel are QEMU-TCG diagnostics, not performance claims. Nothing was run on
the physical laptop.

## Scope

Milestone 0.0.6 replaces the round-robin SMP demo with an EEVDF scheduler and
adds the surrounding machinery needed to make it observable:

1. A host-testable `sketchos-sched-core` crate with a fixed-capacity indexed AVL
   run tree keyed by `(virtual deadline, entity id)`, EEVDF virtual-time and
   deadline arithmetic, a symmetric lag clamp, a log2 latency histogram and a
   fixed-capacity FIFO wait queue. Nothing in the crate allocates.
2. EEVDF scheduling: weighted virtual time, the requested slice treated as a
   deadline request, Linux-style eligibility, and new threads joining at the
   run queue floor instead of at wall-clock time.
3. One fixed-capacity run queue per core. The running entity is removed from
   the tree and still counted in the queue's floor and load-weighted average.
4. Cluster-aware work stealing driven by ACPI PPTT topology: same-cluster
   victims are tried first, the victim's latest-deadline runnable entity is
   taken, and a rate-limited `SGI_BALANCE` lets an idle core ask a busier core
   to push work.
5. A one-shot per-core `CNTV` timer. The next event is the minimum of the
   running entity's slice end, this core's timer waits, the local tree head
   deadline and the demo end. An empty queue disarms the timer and the core
   idles in `WFI` instead of taking a periodic tick.
6. A debug-only invariant checker over the run queues and entity table.
7. Per-thread wake-latency and run-delay statistics: count, sum, maximum and a
   log2 histogram, reported with p50/p99 bucket upper bounds and explicitly
   labelled as QEMU-TCG diagnostics.
8. A latency workload: a 2 ms sleeper (weight 4096, 1 ms request) and a CPU hog
   (weight 256, 20 ms request) in one address space on the boot core.
9. An additive `SYS_SLEEP = 6` system call used by that workload.
10. A bootable FAT32 EFI System Partition image built without touching a
    physical disk, and a smoke-test mode that boots it directly.

The 0.0.5 process model is preserved. User threads remain pinned to the boot
core in this milestone; the migration workload is the kernel demo workers. The
`svc` ABI version stays `0x0001_0000` because `SYS_SLEEP` is additive.

## What did not change

- The UEFI loader, Boot ABI v2, relocatable higher-half kernel, W^X audit,
  recoverable kernel faults, low-half sealing, physical allocator and heap.
- The 0.0.5 process/thread model, per-process address spaces, ELF loader,
  `init`, IPC pair, fault test and thread test. The user phase now contains six
  processes and eight threads because the latency workload was added; the five
  0.0.5 images and their behaviour are unchanged.
- The 0.0.5 syscall numbers 0-5 and their semantics.

## Verified QEMU configuration

All results below come from:

```text
QEMU 10.1.94 (v10.2.0-rc4) on Windows
-machine virt,gic-version=3 -cpu max -smp 8 -m 2048
EDK2 AArch64 firmware (edk2-stable202408-prebuilt)
release build of the 0.0.6 tree, 2026-11-12
```

`-TwoCluster` changes only the QEMU CPU topology to
`-smp 8,clusters=2,cores=4`; the kernel still sees flat MPIDR values 0-7 and
takes the cluster map from PPTT. `-Debug` selects the unoptimised kernel build,
which enables the invariant checker.

| Mode | Command flag | Entry | PPTT | Clusters | Timer events | Result |
| --- | --- | --- | --- | --- | --- | --- |
| Normal | none | EL1 | 10 nodes | 1 | 1146 armed, 15 idle disarms | PASS |
| Two-cluster | `-TwoCluster` | EL1 | 12 nodes | 2 | 272 armed, 15 idle disarms | PASS |
| Framebuffer | `-Framebuffer` | EL1 | 10 nodes | 1 | 288 armed, 15 idle disarms | PASS |
| Relocation | `-OccupyLegacy` | EL1 | 10 nodes | 1 | 252 armed, 15 idle disarms | PASS |
| EL2 entry | `-Virtualization -ExpectEl2` | EL2, then EL1 | 10 nodes | 1 | 264 armed, 15 idle disarms | PASS |
| Debug | `-Debug` | EL1 | 10 nodes | 1 | 285 armed, 15 idle disarms | PASS |
| Standalone image | `-EspImage build/sketchos-esp.img` | EL1 | 10 nodes | 1 | 255 armed, 15 idle disarms | PASS |

Every mode printed `SKETCHOS 0.0.6 KERNEL OK`, requested `PSCI SYSTEM_OFF` on
QEMU, and exited before the timeout. Evidence logs are
`build/evidence-0.0.6-{normal,two-cluster,framebuffer,occupy-legacy,virtualization,debug,image}.log`.

### QEMU 11.1.0 revalidation (2026-11-12)

The same release tree was re-run ten times on QEMU 11.1.0 with the tuned launch
path now in `tools/smoke-test.ps1` (explicit `tcg,thread=multi,tb-size=256`, one
preallocated RAM backend with `mem-lock=off`, 0.5 ms host timer resolution, High
process priority). All ten runs passed. Summary:

```text
inventory        23.7-28.7 ms (median 25.0 ms)
physical         1.56-1.81 us/pair (median 1.65 us/pair)
heap             439-545 ns/pair (median 460 ns/pair)
timer events     1373-1575 armed, 14-15 idle disarms
scheduler        ~15.9-16.2M total work, 170-198 ticks per core
steals/migrations 7 / 7 every run (7 same-cluster, 0 cross-cluster)
```

The earlier untuned 10-run baseline split into two timer-event rate modes
(~1,492-1,590 and ~235-282 events); this batch stayed in the higher-rate mode.
The fixed-cost medians are inside the untuned baseline's spread, so the tuning
is more consistent rather than faster. That is a host scheduling observation,
not a kernel change. Logs and the summary table are in `build/perf-optimized/`.

WHPX was attempted on the same host and failed before the kernel ran: the
bundled EDK2 AArch64 firmware never reaches `BdsDxe`, and a bare-metal probe
shows WHPX faults on `CNTP_CVAL_EL0` while the virtual timer works. The details
and the accelerator constraints are recorded in
[hardware-compatibility.md](hardware-compatibility.md#qemu-acceleration-on-this-host-2026-11-12).
`-Accel whpx` exists only to reproduce that diagnostic failure; TCG remains the
default.

In response, the kernel now uses the **EL1 virtual timer** (`CNTV_*`, PPI 27)
instead of the physical timer (`CNTP_*`, PPI 30): `kernel/src/arch/aarch64/timer.rs`
programs `CNTV_CVAL_EL0`/`CNTV_CTL_EL0` and reads `CNTVCT_EL0`, and
`gic.rs` selects `virtual_el1_gsiv` (falling back to 27). This is the timer a
hypervisor exposes to a guest (WHPX's vGIC wires only `Cntv`), and on bare
metal `CNTVOFF_EL2` is zero so deadlines stay consistent. The smoke-test
assertion now expects `Timer: CNTV one-shot`. The kernel image shrank slightly
(1,840,648 vs 1,842,688 bytes) and the demo now completes in ~32-37 ticks per
core instead of ~188, because the virtual timer fires less frequently under
this QEMU configuration; the smoke test still asserts the generic `[1-9][0-9]*`
tick range and 7/7 steals and migrations.

## EEVDF scheduling

The arithmetic lives in `sched-core/src/eevdf.rs` and is host-tested.

- **Weight.** `WEIGHT_SCALE` is 1024, the neutral point. Running for `t`
  nanoseconds advances virtual time by `t * 1024 / weight`, clamped to at least
  one nanosecond. The accepted weight range is 16 to 65535.
- **Virtual deadline.** `deadline = vtime + slice * 1024 / weight`. The
  requested slice is therefore a deadline request, not a guarantee of
  uninterrupted execution. Slices are clamped to 500 us .. 50 ms.
- **Eligibility.** An entity is eligible when
  `vtime * sum(weight) <= sum(vtime * weight)` over the runnable entities plus
  the running one. This is the Linux `vruntime - min_vruntime <= avg_vruntime`
  test with `min_vruntime` cancelled on both sides; the kernel computes it in
  `u128` so a full table cannot overflow.
- **Lag clamp.** Virtual time is clamped to `floor ± 6 ms` (`LAG_LIMIT_NS`), so
  a sleeper cannot bank unlimited credit and a repeatedly preempted entity
  cannot be pushed arbitrarily far ahead.
- **New threads.** A new entity starts at the run queue's `floor`, not at wall
  clock time. `floor` is the minimum virtual time of the entities assigned to
  that core, including the running one. Starting at `now` would hand a newcomer
  a burst of credit whenever the floor lagged the counter.
- **Choice.** The core picks the eligible tree entry with the earliest
  deadline. If no entry is eligible, it still takes the earliest-deadline entry
  rather than idling while work exists.
- **Yield.** A yield forfeits the remaining slice: virtual time advances by the
  full requested slice. This keeps a yielding thread from gaining an advantage
  over one that runs to its deadline.
- **Preemption.** The running entity is preempted when its slice elapsed, or
  when the tree head is eligible and has an earlier deadline.

### Per-core run queues

Each core owns a `RunTree` with a compile-time capacity. The tree is an indexed
AVL keyed by `(deadline, id)`, so equal deadlines resolve deterministically by
entity id and removal is by exact key. The running entity is **not** in the
tree; it is removed at dispatch. It still contributes to `floor` and to the
load-weighted average, because EEVDF must account for the work the core is
currently doing. `MAX_ENTITIES` is 32.

### Cluster-aware stealing

`sched-core` takes the topology as plain cluster numbers; the kernel derives
them in `kernel/src/topology.rs`.

- A core with no runnable work scans victims in two passes: same cluster first,
  then other clusters.
- From a victim, the thief takes the **latest** deadline runnable entity. That
  is the entity the victim would run last, so taking it is the cheapest steal.
- Affinity is checked before the move. Kernel demo workers are allowed on every
  online core; user entities carry a one-core mask in this milestone.
- A stolen key is removed from the victim tree and inserted into the thief tree
  as one transaction under the scheduler lock. The entity's `core` and
  `runnable_since_ns` are updated, and the steal is counted as same-cluster or
  cross-cluster.
- An idle core with a busier neighbour can request a push with `SGI_BALANCE`
  (INTID 3). The request is rate-limited to one per `BALANCE_COOLDOWN_NS`
  (500 us) per core. `on_balance` pushes the latest-deadline entity to a
  same-cluster idle core when one exists.

Observed in the normal 8-vCPU run: `7 steals, 7 migrations (7 same-cluster, 0
cross-cluster), 0 balance SGIs`. With two PPTT clusters: `7 steals, 7 migrations
(3 same-cluster, 4 cross-cluster), 0 balance SGIs`. The zero `SGI_BALANCE`
count is real: direct stealing from the idle core satisfied every run, so the
push path was not exercised by this workload.

### One-shot timer and tickless idle

`kernel/src/arch/aarch64/timer.rs` programs the non-secure EL1 physical timer
(`CNTP_CTL_EL0`, `CNTP_CVAL_EL0`) with an absolute deadline. There is no
periodic tick.

For each core the scheduler computes the next event as the minimum of:

- the demo end, while the demo is active and this core has not finished;
- the wake time of every sleeping entity assigned to **this** core (the timer
  list is global, but another core's wait must not wake this one);
- `now + remaining_ns` for the running entity;
- the head deadline of this core's run tree, when it is in the future.

If the minimum is `u64::MAX`, the timer is disarmed and the core executes
`WFI`. Every run observed `15 idle disarms`: the seven secondary cores park
once after the demo, and the boot core reaches an idle arm/disarm sequence
during the user phase. The third number in the timer report is the timer's
cumulative disarm count, i.e. how many times `CNTP_CTL_EL0` was written with the
interrupt masked.

When a core finishes its demo, it removes its kernel entities, marks itself
finished and parks in `WFI` with the timer disarmed. It does not keep a timer
armed merely to expire the global demo deadline.

## Debug invariant checker

In a debug build the scheduler calls `check_invariants` after dispatch,
blocking, wakeup, sleep, exit and idle transitions. It verifies:

- every run tree is a valid AVL tree;
- every tree key names a used entity whose `core` is the tree's core, whose
  `deadline` equals the key, and whose state is runnable;
- every runnable entity appears in exactly one tree;
- every running entity appears in no tree, is `current` on exactly one core,
  and is within its affinity mask;
- no non-runnable entity appears in a tree.

A violation prints `Sched: INVARIANT VIOLATION: N problem(s) at check M` and
increments the violation counter; the smoke test fails if that line appears.
Release builds print `Sched: invariant checker: disabled in release build` and
compile the checker out.

Observed debug run: `Sched: invariant checker: 293 checks, 0 violations`. The
count is the number of times the checker ran, not the number of invariants per
call.

## Latency accounting

Each entity carries two `LatencyStats` instances:

- **Wake latency**: from `wake_at_ns` (the requested sleep expiry) to the
  moment the entity is dispatched after waking. Recorded only for user
  entities that actually slept.
- **Run delay**: from `runnable_since_ns` (when the entity last became
  runnable) to dispatch. Recorded on every dispatch, including the first.

`LatencyStats` stores count, sum, maximum and a log2 histogram. The reported
p50 and p99 are **bucket upper bounds**, not interpolated values, and the report
line says so with `p50 <= N ns` / `p99 <= N ns`.

Observed normal run (QEMU-TCG, release):

```text
  pid 1 tid 0: wake n 0, p50 <= 0 ns, p99 <= 0 ns, max 0 ns; run delay n 2, p50 <= 1048576 ns, p99 <= 8388608 ns
  pid 2 tid 1: wake n 0, p50 <= 0 ns, p99 <= 0 ns, max 0 ns; run delay n 2, p50 <= 2097152 ns, p99 <= 2097152 ns
  pid 3 tid 2: wake n 0, p50 <= 0 ns, p99 <= 0 ns, max 0 ns; run delay n 2, p50 <= 524288 ns, p99 <= 2097152 ns
  pid 4 tid 3: wake n 0, p50 <= 0 ns, p99 <= 0 ns, max 0 ns; run delay n 1, p50 <= 4194304 ns, p99 <= 4194304 ns
  pid 5 tid 4: wake n 0, p50 <= 0 ns, p99 <= 0 ns, max 0 ns; run delay n 2, p50 <= 4194304 ns, p99 <= 8388608 ns
  pid 5 tid 5: wake n 0, p50 <= 0 ns, p99 <= 0 ns, max 0 ns; run delay n 1, p50 <= 4194304 ns, p99 <= 4194304 ns
  pid 6 tid 6: wake n 25, p50 <= 1048576 ns, p99 <= 2097152 ns, max 1998300 ns; run delay n 51, p50 <= 2 ns, p99 <= 262144 ns
  pid 6 tid 7: wake n 0, p50 <= 0 ns, p99 <= 0 ns, max 0 ns; run delay n 28, p50 <= 32768 ns, p99 <= 8388608 ns
Sched: latency figures are QEMU-TCG diagnostics, not performance claims
```

`pid 6 tid 6` is the sleeper: 25 requested sleeps, 25 wake samples. `pid 6 tid
7` is the hog: it never sleeps, so its wake count is zero and only run delay is
recorded. A `p50 <= 2 ns` is a histogram bucket, not a measured 2 ns latency.
The values are useful for detecting a broken wake path, not for claiming
scheduler quality.

## Observed evidence

### Normal mode (8 vCPU, 1 cluster)

```text
  SLIT: 0 localities (absent), PPTT: 10 nodes / 0 caches / 1 clusters
Kernel inventory time: 25941 us (25941600 counter ticks at 1000000000 Hz)
Physical latency: 1000 alloc/free pairs in 1625 us (1625 ns/pair)
Heap latency: 1000 64-byte alloc/free pairs in 435 us (435 ns/pair)
Topology: 8 cores in 1 cluster (PPTT)
Scheduler: EEVDF, 11 kernel workers, 16 KiB stacks with guard pages, 4000 us slice request, 1000-ms demo
Sched: 8 per-core run queues, one-shot CNTP events, 1 cluster, cluster-aware stealing
Scheduler: core 0 demo complete, 145 ticks, 145 context switches, 0 steals
Scheduler: core 1 demo complete, 146 ticks, 146 context switches, 1 steals
...
Scheduler: core 7 demo complete, 141 ticks, 141 context switches, 1 steals
Sched: 7 steals, 7 migrations (7 same-cluster, 0 cross-cluster), 0 balance SGIs
Sched: one-shot timer: 1146 events armed, 15 idle disarms (tickless), 1161 IRQ masks
Sched: invariant checker: disabled in release build
SMP: 8 of 8 CPUs online, total work 27256660
SMP: concurrency verified (every online core executed scheduler work)
Userspace: 6 processes, 8 threads, 46 pages, 16 KiB user stacks
latency: long-slice hog done
latency: short-slice sleeper done
Userspace: summary 6 processes, 0 threads
SKETCHOS 0.0.6 KERNEL OK
```

All 11 workers reported `fp ok`; the FP accumulator still matches each worker's
iteration count, so the new context switches preserve `q0..q31`, `FPSR` and
`FPCR`. The 0.0.5 processes still print their expected lines, pid 4 still faults
and only pid 4 is reaped, and every process exits with zero user pages.

### Two-cluster mode

```text
  SLIT: 0 localities (absent), PPTT: 12 nodes / 0 caches / 2 clusters
Topology: 8 cores in 2 clusters (PPTT)
  cluster 0: cores 0 1 2 3
  cluster 1: cores 4 5 6 7
Sched: 8 per-core run queues, one-shot CNTP events, 2 clusters, cluster-aware stealing
Sched: 7 steals, 7 migrations (3 same-cluster, 4 cross-cluster), 0 balance SGIs
Sched: one-shot timer: 272 events armed, 15 idle disarms (tickless), 287 IRQ masks
SMP: 8 of 8 CPUs online, total work 32912964
SMP: concurrency verified (every online core executed scheduler work)
SKETCHOS 0.0.6 KERNEL OK
```

This mode proves two things the flat-MPIDR run cannot: the PPTT decoder follows
parent links to a cluster ancestor (12 nodes, 2 clusters), and the thief orders
victims by cluster, since four steals crossed the boundary only after the
same-cluster pass found nothing to take. QEMU's MPIDRs stay 0-7 in both modes;
the kernel does not infer the cluster split from affinity bits here.

### Bootable image mode

```text
Booting the standalone ESP image: ...\build\sketchos-esp.img
Kernel file: \EFI\SketchOS\kernel.elf (1842688 bytes)
Sched: one-shot timer: 255 events armed, 15 idle disarms (tickless), 270 IRQ masks
Userspace: 6 processes, 8 threads, 46 pages, 16 KiB user stacks
SKETCHOS 0.0.6 KERNEL OK
PASS: kernel reached the success marker and powered off.
```

The image was also booted in debug form from an earlier build during image
bring-up (`Sched: invariant checker: 308 checks, 0 violations`), which is why
the image builder's structural verification is backed by a real guest boot and
not only by a byte comparison.

## Bootable target-device artifact

`tools/make-esp-image.ps1` builds `build/sketchos-esp.img` from `build/esp` in
pure PowerShell. It never opens a physical disk, partition, bootloader or
firmware setting. `tools/build.ps1` runs it after assembling the ESP and copies
the result to `images/sketchos-esp.img`.

The release image observed on 2026-11-12:

```text
size       68157440 bytes (65 MiB)
partition  type 0xEF, LBA 2048, 131072 sectors
FAT32      128992 clusters of 512 bytes, 1024 FAT sectors
loader     52224 bytes, sha256 e0c9c6aa37425ec61c119ea834b91e61b10c6d4f619e0e4a86ff197d9faa2404
kernel     1842688 bytes, sha256 959af6a815866309c25737afdc81f288bce8e0edc1b01d3c6e3bce93d2de429d
image      sha256 d31c8f16ce81747f69f0ebcda88dc3856c854548e702fcca6546e9095f106cbe
verified: MBR, FAT32 geometry, FAT chains and both files
```

The builder re-opens the image, walks the MBR, boot sector and FAT chains, and
compares the SHA-256 of `\EFI\BOOT\BOOTAA64.EFI` and
`\EFI\SketchOS\kernel.elf` with the source files. The smoke test then boots the
same image file, so structural validity and bootability are both observed.

The image is a removable-medium artifact. Writing it to a USB drive erases that
drive; it must be an explicitly named removable device. The internal SSD,
Windows bootloader, UEFI variables and Secure Boot state are out of scope. No
physical boot has been performed, and none should be attempted without explicit
authorization. See [real-hardware.md](real-hardware.md).

## Deliberate deviations

1. **One IRQ-safe scheduler lock.** The design notes for this milestone called
   for per-core locks and a `try_lock` steal path. The implementation uses one
   `IrqSpinLock<State>` for the whole scheduler, and stealing happens under that
   lock. The per-core AVL trees, the one-shot timer and the accounting are real;
   the locking is coarser than planned. This is a correctness-first choice for a
   32-entity boot demo and is **not** a scalability claim. It must be revisited
   before any scheduler-throughput work.
2. **`SGI_BALANCE` is implemented but unexercised.** The push path exists and is
   rate-limited, but every observed run satisfied demand by direct stealing, so
   the balance-SGI count is zero. The smoke test asserts the line and its
   numeric fields, not that the value is non-zero.
3. **User threads remain boot-core pinned.** Cross-core user scheduling, user
   migration and TLB shootdown are not in 0.0.6. The migration workload is the
   kernel demo, whose entities have an all-core affinity mask.
4. **The idle `WFI` path is only observed as disarms.** The log counts disarms
   and reports concurrency, but it does not instrument time spent in `WFI`; no
   idle-power claim is made.

## Host tests

The kernel binary cannot be host-tested (it defines `panic_impl`), so the
command is scoped to the host-testable crates:

```powershell
cargo test -p sketchos-sched-core -p sketchos-elf -p sketchos-mm -p sketchos-driver-api -p sketchos-bootinfo
```

Observed results:

| Crate | Tests passed |
| --- | --- |
| `sketchos-sched-core` | 20 |
| `sketchos-elf` | 6 |
| `sketchos-mm` | 31 |
| `sketchos-driver-api` | 3 |
| `sketchos-bootinfo` | 1 |

`sketchos-sched-core` covers AVL insert/remove/order/duplicate/overflow, EEVDF
weight arithmetic, deadline ordering, symmetric lag clamp, eligibility,
histogram percentiles and wait-queue order.

`cargo test --workspace` fails at the kernel crate with
`error[E0152]: found duplicate lang item panic_impl`. That failure is expected
for a freestanding binary target and is not a test failure.

## Known limitations

- The single scheduler lock described above.
- `SGI_BALANCE` was not exercised in any captured run.
- No user-thread migration, no cross-core user wakeup, no TLB shootdown for user
  address spaces.
- No preemption of kernel entities by user entities beyond the demo-end
  retirement; user scheduling is confined to the boot core.
- Sleeping threads leave their process `ThreadState::Blocked` after they wake.
  The scheduler entity state is correct and the sleep path is what dispatch
  consults, but the process-level state is not refreshed. This is a
  bookkeeping wart, not an observed scheduling failure.
- The timer report's third number is a cumulative disarm count (interrupt-mask
  writes), not a count of distinct interrupts or a latency measure.
- No physical boot, no laptop SMP result, no non-PL011 UART, no storage driver.
- No performance claim of any kind.

## Reproduce

```powershell
# Release build; assembles build/esp, creates build/sketchos-esp.img and
# copies it to images/sketchos-esp.img.
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\build.ps1

# Default 8-vCPU validation.
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -KeepLog

# Reproduce the WHPX diagnostic failure (expected: no kernel banner).
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -Accel whpx -TimeoutSeconds 60

# Two PPTT clusters.
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -TwoCluster -KeepLog

# Debug build with the invariant checker.
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\build.ps1 -Debug
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -Debug -KeepLog

# Boot the standalone image rather than build/esp.
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -EspImage .\build\sketchos-esp.img -KeepLog
```

The other modes are `-Framebuffer`, `-OccupyLegacy` and `-Virtualization
-ExpectEl2`. See [performance.md](performance.md) for the measurement policy and
[smp.md](smp.md) for the scheduler design.
