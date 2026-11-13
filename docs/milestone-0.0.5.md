# Milestone 0.0.5 record

**Status:** complete and verified under QEMU on 2026-11-11.

This record covers what the 0.0.5 kernel and its user images actually do, how
they were verified, and what they deliberately do not do. Every claim below is
backed by a passing `tools/smoke-test.ps1` run in the mode named next to it, or
by a host test named at the end. It does not claim any capability that was not
observed in a log.

## Scope

Milestone 0.0.5 adds a minimal EL0 userspace on top of the 0.0.4 higher-half
kernel. It keeps every 0.0.4 behaviour, runs the unchanged kernel SMP/scheduler
demo, and then hands the boot core to a user phase that provides:

1. A versioned `svc #0` ABI with the call number in `x8`, arguments in
   `x0..x3`, the result in `x0`, and negative Linux-style errno values.
2. Per-process user address spaces built on `AddressSpace`, with TTBR0/ASID
   switching and W^X user pages.
3. A static non-PIE AArch64 ELF loader with a host-tested parser, per-page
   permissions, BSS zeroing and an initial user stack record.
4. A process/thread model: a process owns an address space and its user pages; a
   thread owns a kernel stack and a trap frame.
5. `abi_version`, `exit`, `write`, `yield`, `send` and `recv` system calls.
6. Synchronous, bounded, point-to-point message IPC.
7. Lower-EL fault routing that terminates and reaps only the faulting process,
   then continues with the remaining processes.
8. A minimal `init` plus four test images: a two-process ping/pong pair, a
   deliberate data abort, and a two-thread process.

Shared-memory IPC was deliberately deferred. There is no shared-memory mapping,
no message queue, no wait-any-with-timeout and no general `clone` interface.

The detailed ABI, including exact blocking rules and error codes, is in
[`docs/syscalls.md`](syscalls.md).

## Verified QEMU configuration

All results in this record come from:

```text
QEMU 10.1.94 (v10.2.0-rc4) on Windows
-machine virt,gic-version=3 -cpu max -smp 4 -m 2048
EDK2 AArch64 firmware (edk2-stable202408-prebuilt)
release build of the 0.0.5 tree
```

The four verification modes were run against the same build:

| Mode | Command flag | Entry | PSCI conduit | Kernel tables | User phase | Result |
| --- | --- | --- | --- | --- | --- | --- |
| Normal | none | EL1 | HVC | 24 | 5 processes, 1 timer tick | PASS |
| Framebuffer | `-Framebuffer` | EL1 | HVC | 23 | 5 processes, 2 timer ticks | PASS |
| Relocation | `-OccupyLegacy` | EL1 | HVC | 25 | 5 processes, 1 timer tick | PASS |
| EL2 entry | `-Virtualization -ExpectEl2` | EL2, then EL1 | SMC | 24 | 5 processes, 1 timer tick | PASS |

Each mode ended with `SKETCHOS 0.0.5 KERNEL OK`, printed
`QEMU: requesting PSCI SYSTEM_OFF`, and QEMU exited before the 150-second
timeout. The smoke test requires:

- the 0.0.5 loader and kernel banners,
- the 0.0.4 geometry, W^X audit, both MMU self-tests, and sealed low half,
- all four CPUs online, concurrency verified, and the boot-core handoff line,
- `Userspace: 5 processes, 6 threads, 35 pages, 16 KiB user stacks`,
- every expected `init`, IPC, thread and fault line,
- the fault's ESR/FAR and `terminated by fault, status 139`,
- the summary `5 processes, 0 threads`, and every pid's status line,
- the success marker and a clean power-off.

The captured logs are `build/evidence-normal.log`,
`build/evidence-framebuffer.log`, `build/evidence-occupy-legacy.log` and
`build/evidence-virtualization.log`. They are generated files, not source.

## Static ELF loading

`elf/` is a new `no_std`, dependency-free, host-tested crate. `Image::parse`
never allocates and never trusts a file offset.

Accepted images:

- ELF64, little-endian, version 1, header layout 64/56 bytes.
- `ET_EXEC` and `EM_AARCH64`; `ET_DYN` is rejected.
- One to eight `PT_LOAD` segments.
- `p_filesz <= p_memsz`, non-zero `p_memsz`, file range inside the image.
- `p_vaddr % 4096 == p_offset % 4096`; a larger `p_align` must be a power of two
  and congruent with `p_offset`.
- `p_memsz` must not end above `2^48`.
- No segment may be writable and executable.
- The entry point must be 4-byte aligned and inside an executable segment.

Rejected outright: `PT_DYNAMIC`, `PT_INTERP`, `PT_TLS`. Other program headers,
including `PT_GNU_STACK`, are skipped. A file with no loadable segment is
rejected.

The kernel adds policy on top of the parser:

- It pages in the image span from the lowest page-aligned segment start to the
  highest page-aligned segment end. Each page takes the union of the flags of
  every segment that touches it, so overlapping segments cannot create a W+X
  page.
- It rejects an image whose start is below page 0, whose end overlaps the user
  stack, or whose image plus stack pages exceed the 64-page per-process budget.
- It zeroes every allocated page, copies `filesz` bytes into the right offsets,
  and leaves `memsz - filesz` as zeroed BSS.
- It cleans the D-cache for each page before mapping it and invalidates the
  instruction cache before the first EL0 entry.
- It requires the entry point to be in a page it marked executable.

The user linker script `userspace/user.ld` places `.text` at `0x0040_0000` and
`.rodata` at `0x0040_1000`. The loader does not require that base; it is a
convention of the test images. The kernel rejects an `ET_DYN` image rather than
implementing relocation.

## Address space and EL0 entry

The process's `AddressSpace` owns the TTBR0 root and ASID. The layout is:

| Range | Contents |
| --- | --- |
| `0x0000_0000_0000_1000` and above | Loaded ELF pages |
| `0x0000_0000_7FFF_C000` .. `0x0000_0000_8000_0000` | Thread 0's 16 KiB user stack |
| `0x0000_0000_7FFF_8000` .. `0x0000_0000_7FFF_C000` | Thread 1's 16 KiB user stack |

Image pages are mapped read-only or read/write and executable only when the ELF
segment says so. Data and stack pages are read/write, non-executable. Every
user mapping has `nG = 1` and `PXN = 1`; executable user pages have `UXN = 0`
and non-executable pages have `UXN = 1`. No user page is writable and
executable.

The 0.0.4 boot sealed the low half by setting `TCR_EL1.EPD0` after every
secondary was online. The user phase runs only after the SMP demo has finished
and the secondaries are parked. `process::start_user_phase` therefore calls
`mmu::unseal_low_half` on the boot core, installs the process root with
`AddressSpace::activate`, and enters the first frame with `scheduler_enter`.
Sealing and unsealing each flush the local TLB.

The initial frame is EL0t with `SPSR_EL1 = 0` (interrupts unmasked), the ELF
entry in `ELR_EL1`, `x0`/`x1` set from the spawn arguments, and `SP_EL0` 48
bytes below the stack top. The 48-byte record is `argc = 0`, `argv[0] = NULL`,
`envp[0] = NULL`, an `AT_NULL` pair, and padding. There is no dynamic linker,
TLS or auxiliary vector.

## Process and thread model

| Object | Owns |
| --- | --- |
| Process | One `AddressSpace`, its mapped physical pages, exit status, fault flag, IPC state |
| Thread | One 16 KiB kernel stack with a 4 KiB guard page, one user stack, one saved trap frame |

Limits are `MAX_PROCESSES = 8`, `MAX_THREADS = 16`, two threads per process, 64
pages per process, 64-byte messages and 16 KiB user stacks.

The five test processes are loaded with fixed pids:

| pid | Image | Purpose |
| --- | --- | --- |
| 1 | `init.elf` | ABI query, two output lines, `yield`, clean `exit(0)` |
| 2 | `ipc-recv.elf` | Wait for `ping`, reply `pong`, exit |
| 3 | `ipc-send.elf` | Send `ping`, wait for `pong`, exit |
| 4 | `faulter.elf` | Read unmapped address `0x1000`, expect termination |
| 5 | `threads.elf` | Two threads share the address space and meet at a barrier |

Every thread gets a static kernel stack in `.bss`, with its guard page unmapped
by `process::start_user_phase` before any user code runs. The trap frame lives at
the top of that stack. A process switch calls `AddressSpace::activate`; a switch
between two threads of the same process keeps the current root.

`exit` and an unrecoverable user fault both reap the whole process: its user
pages are freed, its threads become `Exited`, its address-space identity is
destroyed, and blocked peers are woken with `-ESRCH`. The final report shows
each pid's status and that every process owns zero user pages. Page tables are
not reclaimed.

## Scheduling and preemption

The user phase is preemptive round-robin on the boot core only. The timer tick
handler checks `process::user_phase_active()` and diverts to `process::on_tick`
instead of the 0.0.4 demo. The slice is five ticks; a tick saves the interrupted
thread's frame and, at the end of the slice, switches to the next runnable
thread. `yield` performs the same selection immediately.

Secondaries are already parked when the user phase starts, so there is no
cross-core user scheduling, no task migration and no TLB shootdown. The observed
runs completed most of the phase before the first 100 Hz tick; the framebuffer
run recorded two ticks and still produced every expected line in a valid order.
A later normal verification run recorded zero ticks. The two-thread image uses a
user-space barrier (atomic counters plus `yield`) so both threads print before
either exits; this tests the thread model without depending on timer preemption
timing.

## IPC

IPC is a synchronous rendezvous between two processes:

- Messages are 1 to 64 bytes.
- A sender either copies directly into an already-blocked receiver's buffer and
  returns, or leaves one pending message and blocks.
- A receiver consumes the matching pending message, or blocks.
- `recv(0, ...)` accepts any sender; `recv(pid, ...)` accepts only that pid.
- A sender waiting for a different receiver, or a receiver that already has a
  pending message, gets `-EAGAIN`.
- Peer death wakes a blocked peer with `-ESRCH`.
- There is no queue, broadcast, priority, timeout or shared memory.

The observed exchange is:

```text
ipc: receiver waiting
ipc: sender sending ping
ipc: receiver got ping
ipc: receiver sent pong
ipc: sender got pong
```

## Fault handling

0.0.5 installs the lower-EL fault hook that 0.0.4 left unset. An unrecoverable
EL0 fault returns `FaultAction::Terminate`; the exception path then calls
`process::terminate_current`, which prints the ESR and FAR, marks the process
`terminated by fault`, frees its pages and schedules the next thread. It does
not halt the core and does not affect another process.

The deliberate fault produced:

```text
fault: reading an unmapped address
Userspace: pid 4 fault (ESR 0x0000000092000006, FAR 0x0000000000001000), terminating process
```

`0x92000006` is a translation fault at EL0 on a data read; `FAR = 0x1000` is the
requested address. The summary records pid 4 as `terminated by fault, status
139` and all other processes still complete normally. This is the only
intentional user fault in the milestone; the user-stack guard pages are a
structural guarantee and were not overflowed.

## Observed user-phase evidence

From the normal run, after the unchanged 0.0.4 SMP/scheduler demo:

```text
Scheduler: boot core handing off to the user phase
Userspace: 5 processes, 6 threads, 35 pages, 16 KiB user stacks
init: hello from EL0
init: syscall ABI 0x00010000
ipc: receiver waiting
ipc: sender sending ping
fault: reading an unmapped address
Userspace: pid 4 fault (ESR 0x0000000092000006, FAR 0x0000000000001000), terminating process
init: exiting with status 0
Userspace: pid 1 requested exit(0)
ipc: receiver got ping
ipc: receiver sent pong
Userspace: pid 2 requested exit(0)
ipc: sender got pong
Userspace: pid 3 requested exit(0)
threads: A online
threads: B online
threads: B done
threads: A done
Userspace: pid 5 requested exit(0)

Userspace: summary 5 processes, 0 threads, 1 timer ticks
  pid 1: exited, status 0, 0 user pages
  pid 2: exited, status 0, 0 user pages
  pid 3: exited, status 0, 0 user pages
  pid 4: terminated by fault, status 139, 0 user pages
  pid 5: exited, status 0, 0 user pages

SKETCHOS 0.0.5 KERNEL OK
QEMU: requesting PSCI SYSTEM_OFF
```

The line order varies with scheduling; the smoke test checks presence, not a
fixed total order. The important invariants are that both IPC sides complete,
both threads print online and done, the faulting process is the only failure,
all five pids are reaped, and the user phase itself ends cleanly.

## Corrections made during this milestone

1. **`ELR` was advanced twice.** The first dispatcher added `4` to the saved
   `ELR` "past the svc". AArch64 already records the next instruction there, so
   every call resumed one instruction late: `init` skipped the version compare
   and the `write` call after it, and the phase produced no output. An assembly
   dump of the compiled test image confirmed the `svc` sequence. Removing the
   adjustment fixed every call; the ABI now documents that `ELR` needs no
   adjustment.
2. **`yield` inherited `x0`.** The first implementation resumed the yielded
   thread with whatever `x0` held at trap time. It now writes `0`, so the ABI has
   a defined result.
3. **ELF host test data.** The misalignment test set `p_offset = 0x1000` and
   `p_vaddr = 0x401000`, which are congruent, then expected
   `MisalignedSegment`; the parser correctly reported `BadEntry` first. The test
   now uses a genuinely incongruent offset.
4. **Dead duplicates removed.** A second `ABI_VERSION` constant in
   `process.rs`, an unused `push_page` helper, `current_pid`, `abi_version()` and
   `USER_STACK_BOTTOM` were removed so the 0.0.5 build is warning-free.
5. **User-stack ownership.** `spawn_image` now records each thread's own user
   stack top; the initial version hard-coded thread 0's top for every thread.
6. **Versioning.** `sketchos-bootinfo`, `sketchos-uefi`, `sketchos-kernel`,
   `sketchos-mm` and the new `sketchos-elf` are 0.0.5. `sketchos-driver-api`
   keeps its independent 0.1.0 API version.

## Verification commands

From the repository root:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\build.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -TimeoutSeconds 120
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -Framebuffer -TimeoutSeconds 120
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -OccupyLegacy -TimeoutSeconds 120
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\smoke-test.ps1 -NoBuild -Virtualization -ExpectEl2 -TimeoutSeconds 120
cargo test -p sketchos-elf
cargo test -p sketchos-mm
cargo test -p sketchos-driver-api
```

Host test results:

```text
cargo test -p sketchos-elf          ->  6 passed, 0 failed; 1 doc-test passed
cargo test -p sketchos-mm           -> 31 passed, 0 failed
cargo test -p sketchos-driver-api   ->  3 passed, 0 failed (1 ignored doc-test)
```

The ELF tests cover a minimal valid image, dynamic linking and interpreter
rejection, W+X rejection, a non-executable entry, misalignment and truncation,
and wrong machine/type. The `sketchos-mm` tests are unchanged from 0.0.4.

QEMU results, all four modes:

```text
normal          PASS  EL1 entry, HVC, 24 tables, 1 user-phase tick
framebuffer     PASS  EL1 entry, HVC, 23 tables, framebuffer console active, 2 ticks
relocation      PASS  EL1 entry, HVC, 25 tables, legacy address occupied
virtualization  PASS  EL2 -> EL1, SMC, 24 tables
```

## Known limitations

- **No shared memory.** IPC copies messages through the kernel. A shared-memory
  path was split out of 0.0.5 deliberately and is not implemented.
- **No IPC timeouts or queues.** A blocked `send`/`recv` waits until the peer
  acts or dies. There is no signal, cancellation or deadline mechanism.
- **User phase is boot-core only.** Secondaries are parked after the 0.0.4 demo;
  there is no cross-core user scheduling, load balancing or TLB shootdown.
- **Fixed processes.** There is no `fork`, `exec`, `wait`, process creation after
  the boot phase, or way to load another image.
- **Whole-process exit.** A thread cannot exit while its process continues;
  `exit` reaps all threads of the process.
- **No page-table reclamation.** `AddressSpace::destroy` releases the logical
  identity, but the fixed table pool does not take the tables back. User
  physical pages are freed. Repeated process creation would exhaust the pool.
- **No demand paging, copy-on-write or swapping.** Every image and stack page is
  allocated up front.
- **No user `argv`/`envp`, TLS, dynamic linking or filesystem.** The initial
  stack has an empty record. All images are static and embedded in the kernel
  at build time.
- **One blocked receiver per process.** IPC state is per process, not per
  thread; two threads of one process blocking in `recv` is not modelled.
- **`write` can emit a prefix before an error.** The range is copied in 64-byte
  chunks; a fault in a later chunk returns `-EFAULT` after earlier bytes were
  already printed.
- **No FP/SIMD test in userspace.** The trap frame saves `q0..q31` and FP
  control state, and kernel FP is verified, but no user image performs FP work.
- **No drivers, filesystem, network, graphics or shell.** The user images talk
  only to the console and to each other.
- **No performance claim.** The user-phase tick count and all other numbers are
  QEMU TCG diagnostics. A one-tick phase means the work finished before the
  first 100 Hz tick; it is not a latency measurement.
- **The physical laptop has not been booted.** Its SPCR UART is not a PL011 and
  its eight-core SMP path is untested.

## What 0.0.5 does not change

The 0.0.4 kernel demo still runs first and is still verified: four CPUs online,
one IPI per secondary, two guarded kernel tasks per core, 200 ticks and 39
context switches per core, all FP states matching, and the sealed low half
before the user phase. 0.0.5 only adds the handoff, the EL0 phase and the
reporting that replaces the 0.0.4 success marker.
