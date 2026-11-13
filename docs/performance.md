# Performance measurement policy and 0.0.6 diagnostics

SketchOS makes **no performance claims**. Milestone 0.0.6 prints a set of
diagnostics that are useful for detecting functional regressions and gross
changes under QEMU. They are not hardware benchmarks and do not predict laptop
behavior. All scheduler and latency values are QEMU-TCG diagnostics.

## Rules

1. **No claim without a measurement.** A number printed during boot is a
   diagnostic unless it was produced by a benchmark with a stated method.
2. **State the environment.** Every number must name the machine, firmware, CPU
   configuration, build profile, QEMU version (if virtualized), and host.
3. **Separate QEMU from hardware.** QEMU TCG timings do not predict Snapdragon
   X Plus performance. They are useful for detecting regressions in boot logic,
   not for comparing operating systems.
4. **Measure the same workload.** A scheduler iteration count is not a
   benchmark. Any comparison must use the same workload, input and measurement
   window on both systems.
5. **Report distributions, not only averages.** Scheduler latency, interrupt
   latency, page-fault cost and I/O latency need percentiles and worst cases.
6. **No "macOS-level" claim** until the hardware drivers, SMP scheduling,
   memory management and graphics stack exist and a defined benchmark suite has
   been run on the physical laptop.

## Measurement environment

All 0.0.6 numbers below come from one normal-mode run:

```text
QEMU 10.1.94 (v10.2.0-rc4) on Windows
-machine virt,gic-version=3 -cpu max -smp 8 -m 2048
EDK2 AArch64 firmware (edk2-stable202408-prebuilt)
release build, 2026-11-12
log: build/evidence-0.0.6-normal.log
```

Those observations are kept as recorded. The same release tree was later
revalidated on QEMU 11.1.0 with a tuned launch path; that run is described in
[QEMU 11.1.0 optimized TCG revalidation](#qemu-1110-optimized-tcg-revalidation).
The WHPX attempt on the same host failed before the kernel started and is
recorded in
[hardware-compatibility.md](hardware-compatibility.md#qemu-acceleration-on-this-host-2026-11-12).

The other modes (`-TwoCluster`, `-Framebuffer`, `-OccupyLegacy`,
`-Virtualization -ExpectEl2`, `-Debug` and `-EspImage`) were also run on the
same source tree and passed. Their memory, IPI, scheduler and timer diagnostics
are not reproduced here because the normal mode is the reference log.

## QEMU 11.1.0 optimized TCG revalidation

The host is a Windows-on-Arm machine with 8 logical CPUs, so a QEMU launch can
be tuned without changing guest semantics. `tools/smoke-test.ps1` now:

- runs `-accel tcg,thread=multi,tb-size=256` explicitly (one host thread per
  vCPU and a 256 MiB translation-block cache);
- places guest RAM in **one** explicit `memory-backend-ram` with
  `prealloc=on`, and states `-overcommit mem-lock=off`. Preallocation removes
  host page-fault latency from the timed window; leaving the pages unlockable
  matters on a unified-memory host, where pinning 2 GiB would compete with the
  GPU/compositor for the same DRAM;
- raises the QEMU process to `High` priority and asks Windows for 0.5 ms timer
  resolution for the duration of the run, restoring the previous resolution
  afterwards. Windows' default resolution was observed at 1.00 ms on this host;
- captures QEMU's stderr so a launch error is reported instead of disappearing
  into a hidden window.

These are host-side controls, not kernel improvements. The guest now uses the
EL1 virtual timer (`CNTV` one-shot events) rather than `CNTP`, so the same
kernel also runs under a hypervisor that reserves the physical timer; the
one-shot event machinery is unchanged.

Ten back-to-back normal-mode runs on 2026-11-12 (`build/perf-optimized/run-01.log`
through `run-10.log`):

| Metric | Median | Range |
| --- | --- | --- |
| Kernel inventory time | 25,028 us | 23,675-28,742 us |
| Physical alloc/free | 1,654 ns/pair | 1,558-1,805 ns/pair |
| Heap alloc/free | 460 ns/pair | 439-545 ns/pair |
| Timer events armed | 1,531 | 1,373-1,575 |
| Idle timer disarms | 15 | 14-15 |
| Scheduler total work | 16,050,026 | 15,856,434-16,201,581 |
| Ticks per core | 190-192 | 170-198 |
| Sleeper wake max | 3.33 ms | 2.39-4.43 ms |

All ten runs passed, stole 7 entities and migrated 7 (7 same-cluster, 0
cross-cluster, 0 balance SGIs), and produced empty QEMU stderr. The earlier
untuned 10-run baseline split into two timer-event rate modes (~1,492-1,590
events and ~235-282 events); this batch stayed in the higher-rate mode. That is
a host scheduling observation, not a proven effect of any single tuning knob,
and it is not a kernel-speed claim. Inventory, allocator and heap medians are
within the run-to-run spread of the untuned baseline, so the tuned launch is
best described as more consistent, not faster.

Continue to treat the numbers as QEMU-TCG diagnostics. They show that the tuned
launch is stable, not that SketchOS is fast on the laptop.


## What is measured, and how

### Kernel inventory time

- **Method:** read `CNTVCT_EL0` at the start of `kmain` and again after the
  inventory print. Convert with `CNTFRQ_EL0` (1 GHz under QEMU virt).
- **Window:** boot validation, ACPI decode, PPTT decode, MMU setup, framebuffer
  init, PCIe enumeration, firmware inventory and memory-map reporting. It stops
  before the allocators, SMP startup and the scheduler.
- **0.0.6 observation:** `25,941 us`.
- **Caveat:** synchronous console output is inside the window. The framebuffer
  console writes to MMIO and dominates the framebuffer-mode time
  (`145,445 us` in that run). This is not a kernel-speed measurement.

### Physical allocator latency

- **Method:** 1000 sequential single-page `alloc` / `free` pairs through the real
  kernel wrapper and real UEFI map. Time is the counter delta across the loop,
  divided by 1000.
- **0.0.6 observation:** `1625 us` total, `1625 ns/pair`.
- **What it includes:** IRQ masking, spinlock acquire/release, bitmap scan and
  the page scrub on free. Under QEMU TCG, each of those is translated guest code
  running on a host thread.
- **Caveat:** one run, no percentiles, no worst case, no host-load control.

### Physical fragmentation

- **Method:** allocate up to 64 single pages, snapshot free runs, free every
  other page, snapshot again, free the rest, snapshot again.
- **0.0.6 observation:** `9` free runs / largest `471,367` pages before; `41` /
  `471,367` after alternating free; `9` / `471,367` after coalescing.
- **What it shows:** freeing every other page creates 32 extra one-page runs,
  and freeing the rest restores the original run count. It is a correctness aid
  for the bitmap, not a workload fragmentation study.

### Heap latency

- **Method:** 1000 sequential 64-byte aligned allocation/free pairs through the
  kernel heap. Time is the counter delta divided by 1000.
- **0.0.6 observation:** `435 us` total, `435 ns/pair`.
- **What it includes:** IRQ masking, heap spinlock, first-fit list walk, split,
  back-pointer write and coalesce on free.
- **Caveat:** the heap is a general first-fit free list. This number is not
  comparable with a slab allocator, a userspace allocator or a different
  workload.

### Heap growth and accounting

- **Method:** `HeapStats` walks the block list after the self-test.
- **0.0.6 observation:** `512 KiB` total from `2` region additions, `512 KiB`
  free in `1` block, largest `512 KiB`, waste `0 KiB`; `2007` allocations,
  `2007` frees, `2` out-of-memory events.
- **Interpretation:** the two 256 KiB regions are physically adjacent and
  coalesce, so `free_blocks` (1) is less than `region_additions` (2). The two OOM
  events are deliberate (one growth-triggering request and one over-cap request).

### Secondary startup time

- **Method:** the boot core timestamps immediately before each `CPU_ON`, and each
  secondary timestamps when it publishes `STATUS_ONLINE` after GIC and timer
  setup. The difference is converted to microseconds.
- **0.0.6 observation:** core 1 `6563 us`, core 2 `4867 us`, core 3 `11272 us`,
  core 4 `6926 us`, core 5 `106 us`, core 6 `12754 us`, core 7 `19814 us`.
- **Caveat:** QEMU TCG schedules vCPUs on host threads. The same core has been
  observed between roughly 100 us and 20 ms across runs and modes. This is not a
  boot-time or PSCI-latency measurement.

### IPI round-trip

- **Method:** the boot core timestamps the SGI send, a secondary handles the ping
  and sends a pong, and the boot core records the counter delta when the pong is
  acknowledged. One ping is outstanding at a time.
- **0.0.6 observation:** core 1 `35,470,600` ticks, core 2 `5,429,700`, core 3
  `16,800`, core 4 `62,054,600`, core 5 `66,068,400`, core 6 `13,600`, core 7
  `42,800,000` (1 GHz counter).
- **Caveat:** the delta includes host scheduling of both vCPU threads. A
  66-million-tick result does not mean the architectural interrupt path is slow;
  it means the receiving vCPU did not run for 66 ms of guest-counter time. These
  are not interrupt-latency claims and no percentiles are collected.

### EEVDF demo

- **Method:** 11 kernel workers are seeded on the boot cluster with equal
  virtual time and equal deadlines. Each requests a 4 ms slice at weight 1024.
  The demo ends after 1 second of counter time. The final report reads per-core
  tick, switch, steal and work counters after every core parks. Wake latency and
  run delay are not collected for kernel workers.
- **0.0.6 observation:** the one-cluster run reported `7 steals, 7 migrations
  (7 same-cluster, 0 cross-cluster), 0 balance SGIs`, per-core switches between
  139 and 147, and `total work 27,256,660`. The two-cluster run reported `7
  steals, 7 migrations (3 same-cluster, 4 cross-cluster), 0 balance SGIs` and
  `total work 32,912,964`.
- **Interpretation:** this proves EEVDF dispatch, preemption, migration and the
  cluster ordering were reachable in the observed runs. It is **not** a
  throughput or latency benchmark. Work counts differ because QEMU schedules
  vCPUs unevenly and because the demo window is wall-clock, not work-bounded.
  The zero balance-SGI count is real: direct stealing satisfied demand.

### One-shot timer and tickless idle

- **Method:** `arm_next_event` computes the next deadline per core. A successful
  absolute arm increments `events armed`; a call with no event disarms the
  timer, increments the scheduler's `idle disarms`, and the core executes WFI.
  The `IRQ masks` field is the timer's cumulative disarm count, i.e. how many
  times `CNTP_CTL_EL0` was written with the interrupt masked.
- **0.0.6 observation:** normal `1146 events armed, 15 idle disarms, 1161 IRQ
  masks`; two-cluster `272 armed, 15 idle, 287 masks`; debug `285 armed, 15
  idle, 300 masks`; standalone image `255 armed, 15 idle, 270 masks`. The other
  release modes were between 252 and 288 armed.
- **Interpretation:** the event count changes with interleaving and is not a
  tick rate. Fifteen idle disarms appeared in every run: the seven secondaries
  park once after the demo and the boot core enters its idle path during the
  user phase. The kernel does not measure time spent in WFI, so this is not an
  idle-power measurement.

### Scheduler latency accounting

- **Method:** each scheduler entity records `wake_latency` from the requested
  sleep expiry to dispatch, and `run_delay` from `runnable_since_ns` to dispatch.
  `LatencyStats` stores count, sum, maximum and a log2 histogram. The report
  prints p50/p99 **bucket upper bounds**, not interpolated values.
- **0.0.6 observation:** the short-slice sleeper (pid 6 tid 6) recorded 25 wake
  samples with `p50 <= 1,048,576 ns`, `p99 <= 2,097,152 ns`, `max 1,998,300 ns`,
  and 51 run-delay samples with `p50 <= 2 ns`, `p99 <= 262,144 ns`. The hog (pid
  6 tid 7) recorded no wakes and 28 run-delay samples with `p50 <= 32,768 ns`,
  `p99 <= 8,388,608 ns`.
- **Interpretation:** the sleeper's wake path works and its requested 2 ms sleep
  expires near the requested time. The `p50 <= 2 ns` is the smallest histogram
  bucket, not a measured 2 ns dispatch latency. These numbers cannot be compared
  with another system or used as a scheduler-quality claim.

### User phase

- **Method:** the 0.0.5 test images plus the 0.0.6 latency workload run at EL0 on
  the boot core. The final summary reports the number of processes, threads,
  pages and the timer ticks recorded during the phase on the boot core. The
  timer is now one-shot, so a user-phase tick is a slice expiry or a sleep
  wakeup rather than a periodic interrupt.
- **0.0.6 observation:** every mode reported `6 processes, 8 threads, 46 pages`
  and `Userspace: summary 6 processes, 0 threads`; every process ended with zero
  user pages, and pid 4 ended with status 139 after its deliberate fault.
- **Interpretation:** this shows the process model still works on the new
  scheduler. It is **not** a measure of user-thread runtime, IPC cost or
  scheduling latency.

## What is not measured

- Boot time to a usable desktop (there is no desktop).
- Interrupt latency, scheduler latency, or context-switch cost on real hardware.
- User-thread runtime, syscall cost, IPC round-trip cost or page-fault cost.
- Percentiles as exact values: the scheduler report gives bucket upper bounds.
- Worst-case wake latency under sustained load.
- Memory bandwidth, cache behavior, TLB behavior or NUMA effects.
- Allocator behavior under a realistic mixed workload.
- Storage throughput or latency (no storage driver).
- GPU throughput, display frame rate, or power (no GPU driver).
- Time spent in WFI, idle residency, or energy per operation.
- Thermal behavior or battery life.
- SMP scalability on physical hardware, or with more than eight cores.
- Anything on the HP OmniBook X. The laptop has not been booted.

## Why the QEMU numbers must not be quoted as performance

QEMU TCG is a functional emulator. Guest instructions are translated and run on
host threads, and the scheduling of those threads is outside the guest's
control. In one 0.0.6 run the IPI test produced 13,600 ticks for core 6 and
66,068,400 ticks for core 5. Startups spanned 106 us to 19,814 us in the same
run. The scheduler armed 235 to 1146 events across otherwise identical runs. A
number with that spread cannot support a claim about hardware interrupt
latency, boot time or scheduler quality.

The diagnostics are still valuable: an allocator leak, a broken coalesce, a
missing secondary, a failed IPI, a lost runnable entity or a scheduler that
never completes changes the log in an obvious way. That is what the smoke test
checks.

## Future measurement plan

When the corresponding subsystems exist, measure on the physical laptop with a
recorded firmware and build:

1. **Boot phases.** Timestamp loader start, `ExitBootServices`, kernel entry,
   MMU enable, SMP startup, driver probe completion and the first user process.
   Report each phase, not only the total.
2. **Interrupt and scheduler latency.** Use the generic timer and PMU cycle
   counter to record entry-to-handler and wake-to-run latencies; report exact
   p50, p99 and maximum, on an idle system and under load.
3. **Context-switch cost.** Measure a controlled ping-pong between two threads
   with and without FP/SIMD state, and report cycles per switch.
4. **Migration cost.** Measure same-cluster and cross-cluster steals with a
   fixed workload, and report the effect of the 500 us balance cooldown.
5. **Memory.** Measure page allocation/free cost, map/unmap cost, TLB shootdown
   cost, and bandwidth with a fixed working set.
6. **Storage and network.** Use a documented device and a fixed I/O pattern;
   report IOPS, throughput and tail latency.
7. **Graphics.** Measure frame time and GPU utilization once a real driver
   exists. Do not infer GPU performance from a framebuffer console.
8. **Power and thermals.** Record package power and temperature under idle,
   single-thread and all-core workloads.

Every result must include the exact commands, build hash or archive, firmware
version, and host conditions. Results that cannot be reproduced are not claims.
