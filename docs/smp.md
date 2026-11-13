# SMP: discovery, startup, IPIs and per-core EEVDF scheduling

Status: implemented and verified under QEMU in milestone 0.0.3 with four
enabled CPUs, extended to eight vCPUs in 0.0.6, re-verified in 0.0.4/0.0.5 on
the higher-half map, and replaced in 0.0.6 by per-core EEVDF run queues,
cluster-aware stealing and one-shot timers. The physical laptop has eight
enabled Oryon cores but has not been booted. This document describes what the
kernel actually does and the evidence that it works under QEMU.

## Identity model

- A core's hardware identity is its MPIDR affinity value, masked with
  `MPIDR_AFFINITY_MASK`. That is the value MADT reports and the value PSCI and
  GICv3 consume.
- The kernel assigns a separate logical index starting at 0 for the boot core.
  A logical index is **never** passed to PSCI or written to the interrupt
  controller.
- `PerCpu::mpidr` stores the hardware affinity and `PerCpu::index` stores the
  logical index.
- `TPIDR_EL1` points at the executing core's `PerCpu` record, so per-core state
  is reachable without a lookup table.

`MAX_SMP_CORES` is 8. QEMU runs 8 in the verification configuration; the laptop
has 8 enabled cores. A platform with more enabled CPUs than the table prints a
warning and ignores the remainder instead of overflowing or guessing.

## Discovery

`smp::discover` runs after the MMU is enabled and before the GIC is initialized.

1. Read the executing core's MPIDR and make it logical index 0. The boot core is
   identified from hardware, not assumed to be index 0 in the MADT.
2. Walk enabled MADT `GICC` entries only.
3. Skip the boot core, duplicate affinity values, and anything past
   `MAX_SMP_CORES`.
4. For every remaining CPU, initialize its `PerCpu` record: logical index,
   MPIDR, idle status, startup stack top, Rust entry point, zeroed counters.
5. Record the discovered count and the expected online count, then install the
   boot core's `TPIDR_EL1`.

The startup stack is a 16 KiB static array per secondary in `.bss`, aligned to
16 bytes. It is separate from the per-task scheduler stacks.

## Topology from PPTT

`topology::init` runs after discovery and decodes the ACPI PPTT processor
hierarchy.

1. Each PPTT processor node carries a table-relative parent offset at node
   offset +8 and an ACPI processor id at +12. The id is matched to the MADT UID
   so the kernel can map a logical core to its leaf.
2. The decoder walks parent links to find the nearest physical-package or
   cluster container. A leaf directly below a package uses the package as its
   cluster; a system with no such container reports one cluster.
3. If PPTT is absent, incomplete or does not cover an online core, the kernel
   falls back to dividing by the MPIDR Aff1/Aff2 bits, and finally to a single
   cluster. The report always names the source, so a topology claim is never
   invented.
4. The scheduler consumes only `cluster_count()` and `same_cluster(a, b)`.

Under `-smp 8`, QEMU's PPTT has 10 nodes and 1 cluster. Under
`-smp 8,clusters=2,cores=4` it has 12 nodes and 2 clusters: logical cores 0-3
are in cluster 0 and 4-7 in cluster 1. MPIDR values stay 0-7 in both cases, so
the second mode proves the parent-link decode rather than an affinity guess.

## PSCI CPU_ON

`smp::prepare` runs before any secondary starts:

1. Copy the boot core's translation state into every record: `TTBR0_EL1`,
   `TTBR1_EL1`, `TCR_EL1`, `MAIR_EL1`, `SCTLR_EL1`, `CPACR_EL1`, `VBAR_EL1`,
   plus the `KERNEL_VA_BASE` bias that converts the boot record and stack
   addresses to their higher-half aliases.
2. Clean the kernel image, page tables, stacks and per-core records to the point
   of coherency with `DC CVAC`, then invalidate the instruction cache. The
   secondary starts with caches off, so the stub, page tables, stacks and
   records must be in memory, not only in the boot core's data cache.

`smp::start_secondaries` then calls PSCI `CPU_ON` once per discovered secondary:

- Target is the MADT/PSCI affinity value.
- Entry is the **physical** address of the `secondary_entry` assembly stub.
- Context is the physical address of that core's `PerCpu` record.
- The 64-bit function ID form is used only when `PSCI_VERSION` reports 0.2 or
  newer; otherwise the 32-bit form is used. The conduit (HVC or SMC) comes from
  the FADT, never from a guess.
- Each core has a bounded wait of two seconds of generic-counter time. A core
  that fails PSCI, fails GIC setup or never reports online is marked and
  excluded from the scheduler. The boot core never waits forever.

If the firmware is not PSCI-compliant, every secondary is marked failed before
any call is attempted. The kernel continues in single-core mode.

## Secondary entry path

`secondary_entry` is linked into the kernel image and runs with the MMU and
caches off and interrupts masked. Its first instructions only touch registers
and the supplied record.

1. Mask `DAIF` and keep the PSCI context in a callee-saved register.
2. Read `CurrentEL`.
3. **EL2 path:** set `HCR_EL2.RW`, disable EL1 physical-counter and FP/SIMD
   traps in `CNTHCTL_EL2`/`CPTR_EL2`, zero `CNTVOFF_EL2`, and set
   `ICC_SRE_EL2` so EL1 can use the system-register GIC interface. Install the
   EL1 translation state and `TPIDR_EL1`, set `ELR_EL2`/`SPSR_EL2`, install
   `SP_EL1`, and `eret` to EL1h.
4. **EL1 path:** install the same translation state and `TPIDR_EL1`, set `SP`,
   and continue.
5. Invalidate all EL1 TLB entries, enable the boot core's `SCTLR_EL1` (MMU,
   caches), and branch to the Rust entry stored in the record with the high-half
   record address in `x0`.

The first twelve `PerCpu` fields have fixed offsets because the assembly reads
them before the MMU is on. Compile-time assertions in `percpu.rs` pin every
offset.

### Higher-half handoff

A secondary starts with the MMU off at a physical address with a physical stack.
The stub therefore:

1. installs both roots: TTBR0 remains the bootstrap identity map (so the
   physical stack and record stay reachable during the switch) and TTBR1 is the
   kernel's higher-half root;
2. translates `TPIDR_EL1`, the stack and the Rust entry with the
   `kernel_va_start - kernel_phys_start` bias stored in the record;
3. enables the MMU and branches to the high-half Rust entry.

After every online secondary has passed this point, `smp::start_secondaries`
calls `mmu::seal_low_half` on the boot core, which sets `TCR_EL1.EPD0`. The low
half is then unmapped on every core and any stray physical dereference faults.
The kernel prints `MMU: TTBR0 walks disabled; low half sealed`.

In 0.0.6 the boot core later unseals its own TTBR0 walks for the user phase,
after the secondaries are parked. The secondaries stay sealed. See
[User phase on the boot core](#user-phase-on-the-boot-core).

## Per-core initialization

`secondary_main` runs on each secondary after the MMU is enabled:

1. Obtain the installed ACPI platform description and the scheduler's topology
   map.
2. Initialize this core's GIC redistributor and CPU interface:
   - find the redistributor frame whose affinity matches this core's MPIDR,
   - wake the redistributor and wait for children to leave sleep, with a bounded
     spin,
   - configure the timer PPI and the SGIs as Group 1, with a normal priority,
   - enable the system-register CPU interface.
3. Initialize this core's private physical timer in the disarmed state and
   enable its PPI before IRQs are unmasked. A stale firmware deadline must never
   reach an IRQ handler with no scheduler frames built.
4. Stamp the online time, publish `STATUS_ONLINE`, and unmask IRQs.
5. Spin on the scheduler release flag. The wait is short and bounded by the boot
   core's scheduler setup; `WFE` would introduce a lost-wakeup window that
   depends on a later timer interrupt to recover.
6. Enter `sched::start_core`.

The boot core initializes its own redistributor and timer through the same
`gic::init_cpu` path, so the boot core is not a special case in the GIC code.

A secondary failure stores a code, publishes `STATUS_FAILED`, masks interrupts
and parks. Codes currently distinguish a missing platform description, GIC setup
failure and timer setup failure.

## IPIs

The kernel uses GICv3 SGIs through `ICC_SGI1R_EL1`:

| SGI | Name | Use |
| --- | --- | --- |
| 0 | `SGI_PING` | boot core asks a secondary to answer |
| 1 | `SGI_PONG` | secondary answers the boot core |
| 2 | `SGI_RESCHED` | reschedule request |
| 3 | `SGI_BALANCE` | rate-limited push request for the stealing path |

`send_sgi` builds the target list from the hardware MPIDR: Aff0 selects a bit in
the 16-bit target list, and Aff1..Aff3 are placed in their architectural
fields. Aff0 values above 15 cannot be addressed by one SGI write and are
rejected instead of silently targeting the wrong core.

The boot core pings each online secondary once:

1. Snapshot the pong counter and timestamp the send.
2. Send the ping.
3. Wait up to one fifth of a second of counter time for a pong.
4. On success, store the measured round-trip delta in the pinged core's record
   and print it. On timeout, fail the test.

Only one ping is outstanding at a time. A pong carries no sender identity, so a
late reply must never be credited to another core. The IPI test therefore does
not pipeline pings.

The IRQ path acknowledges the interrupt, dispatches SGIs, and takes the
IRQ-safe scheduler lock when it touches scheduler state. No interrupt handler
allocates memory or takes a sleepable lock.

This is a foundation, not a general IPI subsystem: there is no arbitrary
core-to-core messaging API, no work queue and no TLB shootdown yet.

## Per-core EEVDF scheduler

The scheduler lives in `kernel/src/sched.rs`; the arithmetic and containers are
in the host-testable `sketchos-sched-core` crate.

- **One run queue per core.** Each `CoreSched` owns a fixed-capacity indexed AVL
  tree keyed by `(virtual deadline, entity id)`. The key resolves ties
  deterministically and lets removal use the observed key instead of a
  recomputed one.
- **The running entity is outside the tree.** It is removed at dispatch, so it
  cannot be selected twice. It still contributes to the queue floor and to the
  load-weighted average.
- **Weights and slices.** Weight 1024 is neutral. Virtual time advances by
  `ran * 1024 / weight`; a deadline is `vtime + slice * 1024 / weight`. Kernel
  demo workers use weight 1024 and a 4 ms request; user threads use weight 1024
  and 4 ms; the latency sleeper uses 4096 and 1 ms; the hog uses 256 and 20 ms.
- **Eligibility.** The head is eligible when
  `vtime * sum(weight) <= sum(vtime * weight)` over runnable entities plus the
  running one. This is the Linux EEVDF average-vruntime test with
  `min_vruntime` cancelled.
- **Lag clamp.** Virtual time is clamped to `floor ± 6 ms`, both for a sleeper
  returning with credit and for an entity that was repeatedly preempted.
- **New threads** start at the queue floor, never below it.
- **Preemption.** The running entity is preempted when its slice elapses, or
  when the head is eligible and has an earlier deadline. A yield forfeits the
  remaining slice.
- **Affinity.** Kernel workers may run on any online core. User entities carry a
  one-core mask (the boot core) in this milestone.
- **Stacks.** Each entity stack is 16 KiB with a 4 KiB guard page immediately
  below it. The guard is unmapped by `sched::init` before any secondary starts,
  so an overflow faults instead of silently corrupting the neighbouring stack.
  No intentional overflow was performed, so this is a structural guarantee, not
  an observed fault test.
- **FP/SIMD.** Workers perform real FP work so the context switch exercises
  `q0..q31`, `FPSR` and `FPCR`. The accumulator lets the final report check the
  saved bits against the iteration count; a mismatch prints `MISMATCH` and
  prevents the verified banner.
- **Demo end.** When a core's demo window ends, the timer handler does not
  report or power off in interrupt context. It marks the core finished and
  returns a synthetic frame that enters `finish_task` on EL1h with IRQs enabled,
  after `gic::end` has run. The core then removes its kernel entities, disarms
  the timer and parks in `WFI`.

### Work stealing

When a core has no runnable work it scans other cores in two passes: same
cluster first, then other clusters. From a victim it takes the entity with the
**latest** deadline, because that is the one the victim would run last.

- The entity's affinity must include the thief; otherwise the thief tries the
  next victim.
- The key is removed from the victim tree and inserted into the thief tree; the
  entity's `core` and `runnable_since_ns` are updated, and the event is counted
  as same-cluster or cross-cluster.
- An idle core can also request a push with `SGI_BALANCE`. The request is
  rate-limited to one per 500 us (`BALANCE_COOLDOWN_NS`) per core. The receiving
  core pushes its latest-deadline entity to an idle core, preferring one in the
  same cluster.

Observed one-cluster run: `7 steals, 7 migrations (7 same-cluster, 0
cross-cluster), 0 balance SGIs`. Observed two-cluster run: `7 steals, 7
migrations (3 same-cluster, 4 cross-cluster), 0 balance SGIs`. The zero balance
count is an honest observation: direct stealing satisfied every idle core in
these runs, so the SGI push path was not exercised.

### One-shot timer and tickless idle

`kernel/src/arch/aarch64/timer.rs` programs `CNTP_CVAL_EL0` for an absolute
deadline and unmasks the PPI. There is no periodic tick.

For each core the next event is the minimum of:

- the global demo end, while the demo is active and this core has not finished;
- the wake time of each sleeping entity whose `core` is this core;
- `now + remaining_ns` of the running entity;
- this core's run-tree head deadline, when it is in the future.

If there is no such event, the scheduler disarms the timer and the core waits in
`WFI`. Every captured 0.0.6 run reported `15 idle disarms`. The timer report's
third number is the timer's cumulative disarm count (interrupt-mask writes), not
an interrupt count.

## Debug invariant checker

In a debug build, `check_invariants` runs after dispatch, blocking, wakeup,
sleep, exit and idle transitions. It verifies:

- every run tree is a valid AVL tree;
- every tree key names a used, runnable entity assigned to that tree's core,
  with a matching deadline;
- every runnable entity appears in exactly one tree;
- every running entity appears in no tree, is `current` on exactly one core,
  and is within its affinity mask;
- no non-runnable entity appears in a tree.

A violation prints `Sched: INVARIANT VIOLATION` and increments a counter; the
smoke test fails if that line appears. Release builds compile the checker out
and print `Sched: invariant checker: disabled in release build`. The debug
evidence run reported `293 checks, 0 violations`.

## Latency accounting

Each entity records wake latency (requested sleep expiry to dispatch) and run
delay (`runnable_since_ns` to dispatch) with count, sum, maximum and a log2
histogram. The report prints p50/p99 **bucket upper bounds** and the line
`Sched: latency figures are QEMU-TCG diagnostics, not performance claims`.

The 0.0.6 latency workload has two user threads in one address space on the boot
core: a 2 ms sleeper that loops 25 times and a long-slice hog that spins until
the sleeper stops. In the normal run the sleeper recorded 25 wake samples (p50
<= 1,048,576 ns, p99 <= 2,097,152 ns, max 1,998,300 ns) and 51 run-delay
samples; the hog recorded 28 run-delay samples. These are diagnostics, not
scheduling-quality claims.

## User phase on the boot core

The 0.0.6 user phase still does **not** add cross-core scheduling. It starts on
the boot core after the EEVDF demo has completed on every online core and the
secondaries have parked in `WFI`.

- User threads are pinned to the boot core by a one-core affinity mask. They
  are scheduled by the same EEVDF code as the kernel demo, with weight 1024 and
  a 4 ms request.
- A switch activates the target process's TTBR0 root; a switch between two
  threads of one process keeps the current root.
- `yield` selects the next runnable thread immediately. `send`/`recv` can block
  the current thread and switch to another. `sleep` marks the thread sleeping,
  adds a timer wait and switches away.
- When a sleep timer fires, the thread becomes runnable again and joins at the
  queue floor after the lag clamp.
- There is no user migration, no cross-core user wakeup and no TLB shootdown for
  user address spaces. Another core cannot be asked to run a user thread in
  0.0.6.
- The two-thread test image uses a user-space barrier (atomic counters and
  `yield`), so it exercises the thread model without relying on timer
  preemption. The latency image relies on preemption and sleep wakeups.

## Synchronization

- `IrqSpinLock` masks IRQs locally before spinning and restores the previous
  `DAIF` state on release.
- Cross-core ordering uses acquire/release atomics.
- No lock-free algorithm is used beyond atomic counters and status flags.
- The allocator lock order is documented in
  [memory-management.md](memory-management.md): heap then physical, never the
  reverse.

**Deliberate deviation:** 0.0.6 uses **one** `IrqSpinLock<State>` for the whole
scheduler. The design notes called for per-core locks and a `try_lock` steal
path. The per-core AVL trees, run queues, one-shot timers and per-core
accounting are real, but the lock is global, so a dispatch on one core can wait
for a dispatch on another. This is a correctness-first choice for a 32-entity
boot demo and is not a scalability claim. It must be revisited before any
scheduler-throughput work.

## Observed evidence

From the 2026-11-12 0.0.6 normal QEMU run
(`build/evidence-0.0.6-normal.log`, QEMU 10.1.94, `-machine virt,gic-version=3
-cpu max -smp 8 -m 2048`, release build):

```text
MMU: higher half, TTBR1 0x00000000bbcc9000, TTBR0 boot 0x00000000bb147000, 24 tables of 2048, physmap 3 GiB
MMU: W^X audit clean (no writable-executable leaf)
MMU: address-space self-test passed (ASID switch and user page)
MMU: fault self-test passed (unmapped read, read-only write, NX execute)
SMP: 8 discovered, 8 expected online, boot MPIDR 0x0000000000000000, PSCI compliant (HVC)
  core 0: boot, MPIDR 0x0000000000000000, online
  core 1: online, MPIDR 0x0000000000000001, startup 6563 us, GICR 0x00000000080c0000
  core 2: online, MPIDR 0x0000000000000002, startup 4867 us, GICR 0x00000000080e0000
  core 3: online, MPIDR 0x0000000000000003, startup 11272 us, GICR 0x0000000008100000
  core 4: online, MPIDR 0x0000000000000004, startup 6926 us, GICR 0x0000000008120000
  core 5: online, MPIDR 0x0000000000000005, startup 106 us, GICR 0x0000000008140000
  core 6: online, MPIDR 0x0000000000000006, startup 12754 us, GICR 0x0000000008160000
  core 7: online, MPIDR 0x0000000000000007, startup 19814 us, GICR 0x0000000008180000
  IPI core 1 (MPIDR 0x0000000000000001): 35470600 ticks round trip
  ...
IPI: 7 secondaries answered a ping/pong round trip
MMU: TTBR0 walks disabled; low half sealed
  SLIT: 0 localities (absent), PPTT: 10 nodes / 0 caches / 1 clusters
Topology: 8 cores in 1 cluster (PPTT)
Scheduler: EEVDF, 11 kernel workers, 16 KiB stacks with guard pages, 4000 us slice request, 1000-ms demo
Sched: 8 per-core run queues, one-shot CNTP events, 1 cluster, cluster-aware stealing
Scheduler: core 0 demo complete, 145 ticks, 145 context switches, 0 steals
...
Sched: 7 steals, 7 migrations (7 same-cluster, 0 cross-cluster), 0 balance SGIs
Sched: one-shot timer: 1146 events armed, 15 idle disarms (tickless), 1161 IRQ masks
Sched: invariant checker: disabled in release build
SMP: 8 of 8 CPUs online, total work 27256660
SMP: concurrency verified (every online core executed scheduler work)
Scheduler: boot core handing off to the user phase
Userspace: 6 processes, 8 threads, 46 pages, 16 KiB user stacks
...
Userspace: summary 6 processes, 0 threads
SKETCHOS 0.0.6 KERNEL OK
QEMU: requesting PSCI SYSTEM_OFF
```

Every online core completed the demo, every secondary answered its ping, and all
11 worker FP states reported `fp ok`. `SMP: concurrency verified` requires, for
every online core, non-zero work and non-zero scheduler ticks; a timeout can
never print it as verified.

The two-cluster run adds `PPTT: 12 nodes / 0 caches / 2 clusters`, prints
`cluster 0: cores 0 1 2 3` and `cluster 1: cores 4 5 6 7`, and reports `7
steals, 7 migrations (3 same-cluster, 4 cross-cluster)`.

The PSCI conduit depends on where firmware started. Without
`virtualization=on`, EDK2 runs at EL1 and selects `HVC`; with
`virtualization=on`, EDK2 starts at EL2 and selects `SMC`. In the 2026-11-12
runs, normal, framebuffer, relocation and image modes reported `HVC`, and
`-Virtualization` reported `SMC` after entering at EL2 and dropping to EL1. The
value is read from the FADT ARM boot flags, never guessed.

The startup times, IPI deltas, work counts, steal counts and timer-event counts
are QEMU TCG diagnostics. QEMU schedules vCPUs on host threads, so these numbers
include host preemption and vary by orders of magnitude between runs. They are
not interrupt-latency, throughput or boot-time claims. See
[performance.md](performance.md).

## Failure handling summary

| Failure | Behaviour |
| --- | --- |
| Firmware does not report PSCI | All secondaries marked failed; kernel continues single-core and prints the fallback line |
| `CPU_ON` returns an error | Error code stored in the record and printed; that core excluded |
| Secondary does not report online | Bounded two-second timeout, `STATUS_TIMEOUT`, excluded |
| Secondary GIC or timer setup fails | `STATUS_FAILED` with a code, core parks; boot core sees the failure |
| No pong within the IPI timeout | IPI test returns an error; boot does not print a verified result |
| Secondary scheduler does not finish | Bounded wait fails; concurrency is not claimed |
| Run-tree invariant violated (debug) | `Sched: INVARIANT VIOLATION` printed; smoke test fails |
| Steal insert fails | The key is returned to the victim tree and the thief tries the next victim |

## Limits and non-goals

- No CPU hotplug, `CPU_OFF`, suspend or resume.
- One scheduler lock for all cores; no per-core lock and no concurrent steal.
- No user-thread migration, cross-core user wakeup or TLB shootdown.
- No general IPI API beyond ping/pong, reschedule and balance.
- No GICv2 and no ITS/LPI support.
- SGI targeting requires Aff0 < 16.
- At most eight cores in the static tables.
- Per-core scheduler state is logically owned by its core, but the global lock
  is what enforces mutual exclusion today.
- The physical laptop's eight-core configuration is a firmware fact, not a
  verified result. No physical SMP test has been performed.
