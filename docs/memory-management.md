# Memory management

Status: virtual memory was added in milestone 0.0.4 and user address spaces in
0.0.5; the allocators date from 0.0.3. Milestone 0.0.6 changes no allocator
algorithm or user-space layout; it adds per-entity TTBR0 switching in the
scheduler and a sixth embedded user image (the wake-latency workload). All of it
is verified under QEMU. This document describes the design, the invariants, the
lock rules and the observed evidence. It does not claim performance on the
physical laptop.

## Structure

| Layer | Location | Responsibility |
| --- | --- | --- |
| Page-table walker | `mm/src/pagetable.rs` | Host-testable descriptor encoding, mapping, W^X permission bits, ASID and TCR/MAIR values |
| Allocator algorithms | `mm/src/physical.rs`, `mm/src/heap.rs` | Host-testable `no_std` logic, no locking, no kernel dependencies |
| Kernel stage-1 map | `kernel/src/arch/aarch64/mmu.rs` | Final W^X map, 512 GiB physmap, user address spaces, ASID allocation, TTBR0 sealing and unsealing |
| ELF parser | `elf/src/lib.rs` | Host-testable `no_std` static-image validation and segment description |
| User address spaces | `kernel/src/process.rs` | ELF pages, per-thread user stacks, page ownership, IPC buffer copies |
| Loader bootstrap map | `boot/uefi/src/handoff.rs` | Identity root for `boot.S`, higher-half root and physmap for the switch |
| Physical wrapper | `kernel/src/phys.rs` | UEFI map input, reservations, static bitmap storage, IRQ-safe lock, page scrubbing |
| Heap wrapper | `kernel/src/heap.rs` | IRQ-safe lock, growth policy, growth cap, physical-page acquisition |
| Boot self-tests | `kernel/src/selftest.rs` | Real-map allocator and heap checks, latency and fragmentation diagnostics |

Keeping the algorithms in a separate crate means the same code is compiled and
tested on the host with ordinary `cargo test`. The kernel only supplies storage,
the memory map and the lock.

## Stage-1 virtual memory

### Address-space split

The kernel uses a 48-bit virtual address space with a 4 KiB granule and four
translation levels. `TCR_EL1` sets `T0SZ = T1SZ = 16`, so TTBR0 covers the low
half and TTBR1 the high half:

| Range | Owner | Contents |
| --- | --- | --- |
| `0x0000_0000_0000_0000` – `0x0000_FFFF_FFFF_FFFF` | TTBR0 | User address spaces. Empty in the kernel context; holds loaded ELF pages and thread stacks while a process runs. |
| `0xFFFF_8000_0000_0000` – `0xFFFF_BFFF_FFFF_FFFF` | TTBR1 | Kernel image: `KERNEL_VA_BASE + physical` |
| `0xFFFF_C000_0000_0000` – `0xFFFF_FFFF_FFFF_FFFF` | TTBR1 | Linear physical map: `PHYSMAP_BASE + physical` |

The kernel image is linked as a position-independent `ET_DYN` image at virtual
address zero. The loader chooses its physical base and relocates it, so its
runtime virtual address is `KERNEL_VA_BASE + physical`. The physical-to-virtual
bias is therefore `kernel_va_start - kernel_phys_start`.

Every physical byte below `MAX_PHYS` (512 GiB) is reachable through the physical
map. That map is the only way the kernel touches page tables, firmware tables,
the framebuffer, MMIO and the UART: there is no direct physical dereference
after the switch.

`MAX_PHYS` is 512 GiB because the 48-bit high half is 256 TiB wide and the
physmap is placed in its second 256 GiB window; 512 GiB fits with room to spare
and is far above the laptop's 16 GiB. The loader computes the actual
`physmap_size` from the highest physical address it must describe, rounded up to
1 GiB, and the kernel rejects a record whose size exceeds `MAX_PHYS`.

### Final kernel map

After the loader's bootstrap roots are live, `mmu::init` rebuilds TTBR1 from
scratch and installs it. The final policy is:

| Region | Permissions | Attributes |
| --- | --- | --- |
| `.text` | read-only, executable at EL1 | Normal write-back |
| `.rodata` | read-only, non-executable | Normal write-back |
| `.data`, `.bss`, early stack, scheduler/secondary stacks | read/write, non-executable | Normal write-back |
| Kernel image gaps | read/write, non-executable | Normal write-back |
| Physmap, default | read/write, non-executable | Device-nGnRnE |
| Physmap, RAM-like descriptors | read/write, non-executable | Normal write-back |
| Physmap, UART | read/write, non-executable | Device-nGnRnE |
| Physmap, framebuffer | read/write, non-executable | Normal non-cacheable |
| Task-stack guard pages | unmapped | — |

The audit walks every leaf descriptor in the installed TTBR1 tree and fails the
boot if it finds a leaf that is both writable and executable. A clean audit is
printed as `MMU: W^X audit clean (no writable-executable leaf)`.

`boot.S` clears `SCTLR_EL1.WXN`, so W^X is enforced by the per-page
`AP[2]`/`PXN`/`UXN` bits rather than by the global write implies execute-never
bit. This keeps the permission model explicit and auditable.

The kernel's page tables come from an 8 MiB, page-aligned BSS pool (2048
tables). If the map needs more tables, `mmu::init` fails and the kernel keeps
running under the loader's map instead of corrupting memory. Boot uses 24
tables on QEMU.

### Descriptor encoding

- `AP[2]` (bit 7) set means **read-only**.
- `AP[1]` (bit 6) set means **EL0 may access the page**.
- `PXN` (bit 53) stops EL1 instruction fetch; `UXN` (bit 54) stops EL0
  instruction fetch.
- `nG` (bit 11) is clear for kernel mappings (global) and set for user mappings
  (tagged by the TTBR0 ASID).
- `AF` (bit 10) is always set; access-flag faults are not used.
- MAIR indices: 0 Normal write-back (`0xFF`), 1 Device-nGnRnE (`0x00`),
  2 Normal non-cacheable (`0x44`), giving `MAIR_EL1 = 0x0044_00ff`.

An earlier build swapped `AP[2]` and `AP[1]`. QEMU's `get_S1prot` consequently
treated read-only pages as writable-by-EL0 and forced execute-never for EL1
fetches (`xn = pxn || (user_rw & PAGE_WRITE)`), which produced a first-fetch
permission fault. The host tests and the kernel audit now assert the correct
bit positions.

### Address spaces and ASIDs

`mmu::AddressSpace` is the foundation for user processes and has been used by
the process layer since 0.0.5:

- `new_user` allocates an empty page-table root from the same pool and a unique
  ASID. ASID 0 is reserved for the bootstrap identity map.
- ASIDs are allocated from a monotonic atomic counter and masked to the width
  reported by `ID_AA64MMFR0_EL1.ASIDBits`: 8 bits by default, 16 bits when
  supported. The rollover skips zero.
- `map` accepts only `Perm::user` mappings inside the low half.
- `activate` installs the root and ASID in `TTBR0_EL1`, flushes the TLB, and
  returns the previous value; `restore` puts it back.
- `unmap` and `protect` exist for the 0.0.5 loader. The user loader maps whole
  images and stacks in one pass and does not currently unmap or re-protect a
  user page after loading.
- `destroy` releases the logical identity. The tables themselves stay in the
  fixed pool: there is no table reclamation, so destroying an address space does
  not make its tables reusable.

The boot self-test allocates one physical page, maps it read/write into a fresh
address space, activates that space, writes and reads `0x5a17_c0de_dead_beef`
through the user virtual address, restores the bootstrap root, and frees the
page. It prints `MMU: address-space self-test passed (ASID switch and user
page)`.

### User address spaces (since 0.0.5)

Each user process owns one `AddressSpace` and the physical pages mapped into it.
`process::spawn_image` and the ELF loader populate it:

- The kernel walks every page of the image span from the lowest page-aligned
  segment start to the highest page-aligned segment end. It allocates one
  physical page per virtual page and maps it with the union of the flags of every
  `PT_LOAD` segment that touches it. Overlapping segments therefore cannot
  produce a writable-executable page.
- It zeroes each page, copies `filesz` bytes to the right offset, and leaves
  `memsz - filesz` as zeroed BSS. It cleans the D-cache before mapping and
  invalidates the instruction cache before the first EL0 entry.
- It allocates four pages per thread for a 16 KiB user stack and maps them
  read/write and non-executable. Thread 0's stack sits directly below
  `USER_STACK_TOP = 0x8000_0000`; thread 1's stack sits below it. The mapping
  records each thread's own stack top.
- It rejects an image that starts below page 0, ends inside or above the stack
  region, or does not fit the 64-page per-process budget with its stacks.

The observed 0.0.6 load was `6 processes, 8 threads, 46 pages, 16 KiB user stacks`.
Each single-thread image takes two pages, and every thread adds four stack pages;
the two-thread `threads` image and the two-thread latency workload account for the
rest. The 0.0.6 latency workload is the sixth embedded image.

`exit` and an unrecoverable EL0 fault free every user physical page owned by the
process and mark its threads `Exited`. The final report shows `0 user pages` for
every pid, which is how the test verifies that ownership accounting is complete.
The address space's root and lower-level tables are not returned to the pool.

IPC does not share memory. `send` and `recv` copy at most 64 bytes through the
kernel between two process address spaces, using validated user ranges. A blocked
sender's message is held in a fixed kernel buffer, not in either user space, so
the sender may reuse its buffer as soon as the call returns. See
[syscalls.md](syscalls.md) for the exact rules.

### Recoverable faults

The synchronous-exception path can resume instead of halting the core. Two
cases are recoverable:

- **Kernel probes.** `probe_read`, `probe_write` and `probe_execute` set a
  resume label and an active flag before the access. If the access aborts, the
  handler rewrites `ELR` to the resume label and the probe returns `Err`.
- **Lower-EL faults.** When a fault comes from EL0, the handler builds a
  `FaultInfo` (vector, `ELR`, `SPSR`, `ESR`, `FAR`, `from_user`) and calls the
  registered `UserFaultHook`. The hook returns `Retry`, `Skip` (resume at
  `ELR + 4`) or `Terminate`. 0.0.4 leaves the hook unset; 0.0.5 installs the
  process layer's handler.

Everything else is reported and halts the core. The probe state is global and
the probes are intended for the boot core before SMP; they are not
multi-core-safe.

The boot fault self-test deliberately makes three faults on the installed map:
a read from an unmapped low address, a write to read-only `.text`, and an
instruction fetch from writable `.bss`. All three must return `Err`; the kernel
prints `MMU: fault self-test passed (unmapped read, read-only write, NX execute)`.
`probe_execute` restores the caller's `x30` on both the success and fault paths,
because a faulting `BLR` may already have overwritten it.

### Low-half sealing and unsealing

Secondaries start with the MMU off on a physical stack and need the bootstrap
TTBR0 identity map to reach the higher half. Once every secondary is online,
`smp::start_secondaries` calls `mmu::seal_low_half`, which sets
`TCR_EL1.EPD0`. TTBR0 walks are then disabled on that core and the low half is
gone: any stray physical dereference faults instead of silently aliasing kernel
memory. The kernel prints `MMU: TTBR0 walks disabled; low half sealed`.

The 0.0.5 and 0.0.6 user phase runs only after the SMP/scheduler demo has finished
on the boot core; the secondaries are parked by then. `process::start_user_phase`
calls `mmu::unseal_low_half` on the boot core, clears `EPD0`, and activates the first
process root. Sealing and unsealing each flush the local TLB. Only the boot core
is unsealed; the secondaries keep TTBR0 disabled for the rest of the run.

### Task-stack guard pages

Every scheduler task stack has an unmapped 4 KiB page immediately below it.
`sched::init` splits the covering block mapping and unmaps each guard before any
secondary starts. The guard is inside the higher-half image mapping, so the rest
of the stack keeps its Normal write-back attributes.

User threads do not get a guard page in 0.0.5 and 0.0.6: each 16 KiB user stack is
four mapped pages, and thread 0's stack sits directly above thread 1's, with no
unmapped page between them. An overflow of thread 0's stack therefore writes
into thread 1's stack instead of faulting; only the page below thread 1's stack
is unmapped. The deliberate fault test targets an unmapped address, not an
overflow. Kernel stacks, in contrast, are guarded.

Stacks are held in `UnsafeCell` wrappers so they land in `.bss`. An immutable
`static` array lands in `.rodata`, which the final map makes read-only; the
first trap-frame write then faults. This was observed during 0.0.4 bring-up for
both `sched::STACKS` and `smp::SECONDARY_STACKS` and fixed by the wrapper.

## Physical page allocator

### Model

- Page size is 4 KiB. All allocations and frees are page-granular and page
  aligned.
- Two bitmaps cover one page-number span:
  - `managed` marks pages the kernel is allowed to hand out.
  - `allocated` marks pages currently owned by a caller.
- A page is free exactly when it is managed and not allocated.
- The span is `highest usable end - lowest usable start`, not the highest
  physical address in the firmware map. The laptop's MMIO windows sit far above
  RAM; indexing by absolute physical address would waste bitmap space on
  addresses that can never be allocated.
- Storage is a caller-supplied `[u64; WORDS]`. The kernel places it in BSS; host
  tests instantiate a smaller constant. The allocator never allocates metadata.

### Initialization and reservations

`phys::init` copies only `CONVENTIONAL` descriptors from the final UEFI map into
a fixed 128-entry array, then reserves explicitly:

- page zero,
- the kernel image (`kernel_phys_start..kernel_phys_end`),
- the `BootInfo` record,
- the copied memory-map descriptor array,
- the loader image, when present,
- the ACPI RSDP page, when present,
- the framebuffer range, when present,
- the UART page, when present.

Only conventional memory is managed. ACPI reclaim, loader, runtime, MMIO,
firmware and unavailable regions are never managed at all. The boot self-test
additionally checks both ends of every non-conventional descriptor, so an
off-by-one in the reserve path fails the boot instead of silently handing out
firmware memory.

A reservation clears `managed` bits. It cannot take a page that is currently
allocated: `reserve_range` validates the whole range first and returns
`ReserveAllocated` without changing anything.

### Allocation, free and errors

- `alloc_aligned(count, align)` finds `count` contiguous free pages whose
  physical address satisfies `align`. Alignment must be a power of two and at
  least 4 KiB; an invalid alignment returns `None` and leaves state untouched.
- The scan starts at a hint and wraps once. The hint is only a performance aid;
  the result remains first-fit.
- The scan skips whole words that are empty in `managed & !allocated`, which
  keeps a mostly allocated bitmap cheap to search without a free list.
- `free_pages` validates the entire range before changing a bit. One unmanaged
  page returns `InvalidAddress`; one managed but unallocated page returns
  `DoubleFree`. That distinction is the reason for two bitmaps.
- All address arithmetic is rounded and checked with `checked_*` operations.
  Overflow, misalignment, out-of-span addresses, too many ranges and too-small
  storage all return typed errors.

### Page scrubbing

`phys::free_pages` zeroes the range after the allocator accepts the free and
before the allocator lock is released. A rejected free is not scrubbed, because
the range may not belong to the caller. Because the lock is held during the
scrub, no core can claim the pages until every byte is zero.

This is confidentiality hygiene at the userspace boundary: an exited process's
pages are zeroed before another process can receive them. It is not
zero-on-allocation semantics for the current kernel: the allocator does not
promise freshly allocated pages are zero unless they came through this free
path.

### Accounting and fragmentation

`Stats` reports span pages, managed pages, allocated pages, free pages, reserved
pages and the managed address range. `Fragmentation` reports free pages, the
number of free runs and the largest free run. Both are `O(span)` and intended for
diagnostics and tests, not for the allocation hot path.

### Bitmap sizing

`WORDS = 131_072` words per bitmap covers a 32 GiB span. Two bitmaps therefore
occupy 2 MiB of BSS. The target laptop has 16 GiB of RAM; the headroom covers a
firmware map whose usable span is slightly larger than RAM. A machine whose
usable span exceeds 32 GiB fails initialization with `StorageTooSmall` rather
than silently managing a subset.

## Kernel heap

### Layout and algorithm

- The heap is an address-ordered first-fit free list over one or more regions.
- Every block starts with a `BlockHeader` containing size, previous and next
  links, the user pointer, a free flag and a magic value.
- Blocks are linked in physical address order. Coalescing with the previous and
  next neighbour is therefore `O(1)` in pointer operations.
- An allocation writes the header pointer into the eight bytes immediately
  before the returned user pointer. This back-pointer recovers the header for any
  power-of-two alignment without a side table or a second allocator. The cost is
  eight bytes per allocation.
- A successful allocation splits the block when the remainder can hold a full
  header; otherwise the whole block is handed over.
- `add_region` rejects null, too-small and overlapping regions, inserts the new
  block in address order and coalesces immediately.

### Validation and failure behaviour

- `alloc` returns `None` on exhaustion, increments `oom_count`, and does not
  modify the heap. `size == 0` and non-power-of-two alignment also return `None`.
- `dealloc` checks, in order: non-null pointer, pointer inside a live block,
  readable non-null back-pointer, header inside a live block, header magic,
  not already free, and the stored user pointer matches. Any failure returns a
  typed error and leaves the heap untouched.
- The region overlap check prevents a region from claiming memory already owned
  by another region.

### Growth policy

- Initial region: 256 KiB.
- Growth happens only after a normal allocation fails.
- The next region is sized to the failed request plus alignment and header slack,
  with a 256 KiB minimum. A fixed chunk would never satisfy a request larger than
  one chunk and would be re-added on every retry.
- The heap is capped at 8 MiB. A request that cannot fit under the cap fails
  before any physical pages are taken. The cap is re-checked under the heap lock
  after the physical pages are acquired; if another allocation grew the heap in
  the meantime, the pages are returned.
- If `add_region` fails, the acquired physical pages are freed.
- Physical pages are acquired with the heap lock released. This avoids holding
  the heap lock across the physical allocator and keeps the lock order simple.

### Locking and interrupt rules

- Both allocators are wrapped in `IrqSpinLock`. Acquiring either lock masks IRQs
  on the local core before spinning, so an interrupt handler cannot deadlock
  against a lock its own core already holds.
- The only permitted nesting order is **heap -> physical**. Nothing acquires
  physical memory and then the heap.
- No interrupt handler allocates from either allocator. The timer, IPI and
  exception paths only touch preallocated per-core state and atomics.
- Heap growth takes physical pages with the heap lock released, so the
  heap -> physical order is not held across the whole operation even though the
  direction is fixed.

### Accounting

`HeapStats` reports total, allocated and free bytes, free and allocated block
counts, the largest free block, the number of `add_region` calls, and allocation,
free and out-of-memory counters. `region_additions` counts calls, not disjoint
regions after coalescing: two physically adjacent additions merge into one free
block, so a healthy heap can have `free_blocks < region_additions`.

`HeapFragmentation` reports free bytes, the largest free block and waste bytes
(free memory outside the largest block).

## Self-tests

The boot self-test runs before SMP starts, so no other core can race it.

Physical allocator checks:

1. managed pages exist and the reported span is sane;
2. a 4 KiB allocation is aligned, writable and readable at three offsets;
3. an invalid free and a double free are distinguished;
4. a 16-page 64 KiB-aligned allocation is aligned and writable;
5. 1000 single-page alloc/free pairs are timed;
6. a 64-page fragmentation sequence is measured before, after alternating frees
   and after full coalescing;
7. the self-test returns the allocator to its starting allocated count;
8. the kernel image, boot record, memory map and both ends of every
   non-conventional descriptor are outside the managed set;
9. a page is filled with `0xa5`, freed, and read back at three offsets; all three
   must be zero.

Heap checks cover small, large, aligned, repeated, growth, growth-cap,
invalid-free and double-free cases. The self-test frees every allocation it
makes. The growth-cap check verifies both that the request fails and that
`total_bytes` did not change, so the failure did not reserve physical memory.

Host tests in `sketchos-mm` additionally cover non-contiguous ranges, reserved
pages, reservation of allocated memory, exhaustion and recovery, coalescing,
overlapping regions, region storage limits, and a 2000-iteration alloc/free
loop.

## Observed evidence

This section preserves the 0.0.5 memory evidence. The 0.0.6 run (6 processes,
8 threads, 46 pages) is recorded in
[`milestone-0.0.6.md`](milestone-0.0.6.md).

From the 2026-11-11 0.0.5 normal QEMU run
(`build/evidence-normal.log`, QEMU 10.1.94, `-machine virt,gic-version=3
-cpu max -smp 4 -m 2048`, release build):

```text
MMU: higher half, TTBR1 0x00000000bbd1e000, TTBR0 boot 0x00000000bb229000, 24 tables of 2048, physmap 3 GiB
MMU: W^X audit clean (no writable-executable leaf)
Kernel: physical 0x00000000bba29000-0x00000000bc52f000 (11288 KiB), virtual 0xffff8000bba29000-0xffff8000bc52f000
  running at 0xffff8000bba38288, inside the higher-half image: yes
  higher half: physmap 0xffffc00000000000-0xffffc000c0000000 (3 GiB), boot root 0x00000000bb229000, kernel root 0x00000000bb22f000
  TCR 0x00000005b5103510, MAIR 0x00000000004400ff
Physical memory: managed 1992 MiB (510196 pages), free 1992 MiB, span 0x0000000040000000-0x00000000bffff000
Physical latency: 1000 alloc/free pairs in 1552 us (1552 ns/pair)
Physical fragmentation: 9 free runs / largest 471593 pages before, 41 / 471593 after alternating free, 9 / 471593 after coalescing
Physical self-test: PASS (alloc, free, alignment, invalid-free, double-free, scrub, reservations)
MMU: address-space self-test passed (ASID switch and user page)
MMU: fault self-test passed (unmapped read, read-only write, NX execute)
Heap latency: 1000 64-byte alloc/free pairs in 401 us (401 ns/pair)
Heap: 512 KiB from 2 region additions, 512 KiB free in 1 blocks, largest 512 KiB, waste 0 KiB
Heap counters: 2007 allocations, 2007 frees, 2 out-of-memory events
Heap self-test: PASS (small, large, aligned, repeated, growth, growth cap, invalid-free, double-free)
MMU: TTBR0 walks disabled; low half sealed
Scheduler: boot core handing off to the user phase
Userspace: 5 processes, 6 threads, 35 pages, 16 KiB user stacks
...
Userspace: summary 5 processes, 0 threads, 1 timer ticks
  pid 1: exited, status 0, 0 user pages
  pid 2: exited, status 0, 0 user pages
  pid 3: exited, status 0, 0 user pages
  pid 4: terminated by fault, status 139, 0 user pages
  pid 5: exited, status 0, 0 user pages
```

The lower managed-page count than 0.0.3 (1992 MiB versus 2006 MiB) is expected:
the 0.0.5 kernel image, loader tables and handoff pages are larger and are all
reserved. The image grew from 1,494,080 bytes in the final 0.0.5 build; the
loader reported 2,822 pages for it.

The two out-of-memory events are deliberate: one allocation exceeds the initial
region to force growth, and one exceeds the 8 MiB cap. The first grows the heap
from 256 KiB to 512 KiB; the second changes nothing.

The latency lines are QEMU TCG diagnostics, not hardware benchmarks. See
[performance.md](performance.md) for the measurement method and caveats.

Host tests:

```text
cargo test -p sketchos-elf          ->  6 passed, 0 failed (1 doc-test passed)
cargo test -p sketchos-mm           -> 31 passed, 0 failed
cargo test -p sketchos-driver-api   ->  3 passed, 0 failed (1 ignored)
```

The page-table tests cover descriptor round-trips, block splitting, `map_range`
alignment, the `AP[2]`/`AP[1]` bit positions, W^X across all four permission
classes, and the 48-bit TCR/MAIR values. The ELF tests cover a minimal valid
image, dynamic linking and interpreter rejection, W+X rejection, a
non-executable entry, misalignment and truncation, and wrong machine/type.

## Limits and non-goals

- Only `CONVENTIONAL` firmware memory is managed. ACPI reclaim and similar
  regions are intentionally excluded until a policy exists for them.
- The bitmap covers a fixed 32 GiB span; a larger usable span fails
  initialization.
- The page-table pool is a fixed 8 MiB BSS region. There is no table
  reclamation: destroying an `AddressSpace` does not return its tables. User
  physical pages are freed, but repeated process creation would exhaust the
  pool.
- There is no demand paging, copy-on-write, swap, NUMA policy, or TLB shootdown
  across cores. `AddressSpace::activate` flushes the local TLB only.
- User address spaces exist, but only six fixed images are loaded at boot. There
  is no `fork`, `exec`, shared memory or way to create another process at run
  time. The user phase runs on the boot core only.
- All user image and stack pages are allocated up front. A process is limited to
  64 pages and two threads through 0.0.6.
- IPC copies messages through the kernel; it never maps the same physical page
  into two processes.
- Freed pages are scrubbed, but allocated pages are not guaranteed zero.
- The heap is not a slab allocator and has no size classes; it is a general
  first-fit free list intended for a small kernel.
- The 8 MiB heap cap is a safety bound for this milestone, not a final heap
  design.
- No performance claim is made for either allocator on real hardware.
