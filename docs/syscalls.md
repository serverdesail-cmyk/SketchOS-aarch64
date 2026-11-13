# SketchOS 0.0.6 system-call ABI

Status: implemented and verified under QEMU in milestone 0.0.6. This document is
normative for the kernel in `kernel/src/syscall.rs` and the user runtime in
`userspace/runtime.rs`.

The ABI is deliberately small. It is versioned, but there is no compatibility
promise beyond the version check below: 0.0.6 is a bring-up interface. The ABI
version stays `0x0001_0000` because `sleep` is an additive call; the existing
calls keep their 0.0.5 numbers and semantics.

## Calling convention

| Item | Register |
| --- | --- |
| Call number | `x8` |
| Argument 0 | `x0` |
| Argument 1 | `x1` |
| Argument 2 | `x2` |
| Argument 3 | `x3` |
| Result | `x0` |
| Trap instruction | `svc #0` |

Arguments are 64-bit unsigned values. The result in `x0` is interpreted as a
signed 64-bit integer:

- `0` or a positive value is success (for `write`, `send` and `recv` it is a
  byte count; for `exit` there is no result).
- A negative value is an error and equals `-errno`.

The kernel returns with the other general-purpose registers unchanged from the
trap frame. A call that blocks returns a *different* thread's frame; the resumed
thread sees its own saved registers, with `x0` holding the result.

`ELR_EL1` already contains the address of the instruction after `svc #0` when
the exception is taken, so a normal return resumes at the next instruction. The
kernel does not adjust `ELR` for a successful call.

## Version

Call 0 returns the ABI version:

```text
abi_version() -> 0x0001_0000
```

The value encodes major 1, minor 0. A user program should compare the whole
value and refuse to run if it does not match. There is no negotiation: an
unknown call number returns `-ENOSYS` and prints a kernel diagnostic line.

## Calls

### 0: `abi_version`

```c
long abi_version(void);
```

Returns `0x0001_0000`. Arguments are ignored. This call has no side effects.

### 1: `exit`

```c
void exit(uint64_t status);
```

Terminates the **whole calling process**, not only the calling thread. All of
its threads become `Exited`, its user pages are returned to the physical
allocator, and the status is recorded in the final summary. `exit` never returns
to the caller; the kernel resumes another runnable thread, or finishes the user
phase if none is runnable.

A process that exits while a peer is blocked in `send` or `recv` wakes that peer
with `-ESRCH`. Its address-space page tables are abandoned in the kernel's fixed
pool (see the limitations).

### 2: `write`

```c
long write(uint64_t descriptor, const void *buffer, uint64_t length);
```

Writes `length` bytes from the calling process's memory to the kernel console.

| Descriptor | Meaning |
| --- | --- |
| `1` | standard output |
| `2` | standard error |

Both currently go to the same synchronous console (serial and/or framebuffer).
Descriptor `0` and any other value return `-EBADF`.

- `length == 0` returns `0` without touching the pointer.
- `length > 4096` returns `-EINVAL`; a single call is bounded.
- The kernel validates and copies the range in 64-byte chunks through the
  process's current address space. An unmapped, non-user or unreadable byte
  returns `-EFAULT`.
- Bytes copied before a later chunk faults have already been printed. The
  return value is then the error, not a partial count. User programs should
  pass a range they own for its whole length.
- Output is not buffered and is not atomic across processes.

### 3: `yield`

```c
long yield(void);
```

Makes the calling thread runnable and switches to the next runnable thread on
the boot core. Returns `0`. If no other thread is runnable, the caller continues
immediately. `yield` does not block and has no failure mode.

### 4: `send`

```c
long send(uint64_t target_pid, const void *buffer, uint64_t length);
```

Sends a bounded message to another process. This is a synchronous rendezvous,
not a queue:

- `target_pid` must name a live process other than the caller.
  - `0`, an out-of-range value or a dead process returns `-ESRCH`.
  - Sending to the caller's own process returns `-EINVAL`.
- `length` must be `1..=64`; otherwise the call returns `-EMSGSIZE`.
- The kernel copies the source range before any blocking, so the caller may
  reuse the buffer after the call returns. An invalid source range returns
  `-EFAULT`.
- If the target is blocked in `recv` and accepts this sender, and its buffer is
  large enough, the kernel copies the message directly into that buffer, wakes
  the receiver with the byte count, and `send` returns the same byte count
  immediately.
- If the target is blocked in `recv` but is waiting for a different sender, the
  call returns `-EAGAIN`; the receiver keeps waiting.
- If the target is blocked in `recv` but its buffer is too small, the receiver
  is woken with `-EMSGSIZE` and `send` returns `-EMSGSIZE`.
- If the target is not blocked and already holds an undelivered message, the
  call returns `-EAGAIN`.
- Otherwise the message is stored as the target's single pending message and
  the sender blocks. It wakes with the byte count when the receiver consumes the
  message, or with `-ESRCH` if the receiver exits or faults first.

A process can have at most one pending incoming message. There is no send queue,
no broadcast, no priority and no timeout.

### 5: `recv`

```c
long recv(uint64_t from_pid, void *buffer, uint64_t length);
```

Receives one bounded message into the calling process's memory.

- `length` must be `1..=64`; otherwise the call returns `-EMSGSIZE`.
- `from_pid == 0` accepts a message from any sender. Any other value accepts a
  message only from that process.
- The destination range is validated before the call blocks. An unmapped,
  non-user or non-writable byte returns `-EFAULT`.
- If a matching pending message exists and `length` is at least the message
  length, the kernel copies it, wakes the blocked sender with its byte count,
  removes the pending message, and returns the message length.
- If a matching pending message is larger than `length`, the call returns
  `-EMSGSIZE` and leaves the message pending.
- If no matching message exists, the calling thread blocks. It wakes with the
  byte count when the matching `send` completes, with `-EMSGSIZE` if the sender's
  message is too large for the buffer, or with `-ESRCH` if the expected peer
  exits or faults.
- A pending message from a different sender does not satisfy a directed `recv`;
  it remains pending for a later call.

IPC state is recorded per process, not per thread. The tested model has one
blocked receiver per process; two threads of the same process calling `recv`
concurrently is undefined in 0.0.6.

### 6: `sleep`

```c
long sleep(uint64_t duration_ns);
```

Blocks the calling **thread** for approximately `duration_ns` of generic-counter
time and returns `0` when the requested duration has elapsed.

- `duration_ns == 0` is equivalent to `yield`: the thread stays runnable and the
  call returns `0` without a timer wait.
- `duration_ns` is clamped to 10 seconds. There is no error return for an
  oversized value.
- The thread is removed from the run queue and a timer wait is recorded for its
  current core. The scheduler's next-event calculation includes that wait and
  the core arms its one-shot `CNTP` timer for the earliest event.
- When the timer fires, the thread becomes runnable again, joins its run queue
  at the queue floor (after the lag clamp) and is dispatched by the normal EEVDF
  rule.
- The kernel records a wake-latency sample from the requested expiry to the
  actual dispatch. That sample appears in the boot report as a histogram bucket
  upper bound.
- The call is not interruptible and has no timeout other than its duration:
  there is still no signal or cancellation mechanism. A short wake due to a
  spurious or unrelated local timer event re-enters the normal dispatch path;
  the entity wakes only when its recorded wake time has been reached.
- If the timer table is full, the kernel degrades to a plain yield rather than
  sleeping forever. The call still returns `0`.
- `sleep` is available only to user threads. The kernel demo does not use it.

## Error codes

| Value | Name | Used by |
| --- | --- | --- |
| `-3` | `ESRCH` | `send` to a missing/self-invalid target, peer death, no current process |
| `-9` | `EBADF` | `write` with a descriptor other than 1 or 2 |
| `-11` | `EAGAIN` | `send` when the receiver is busy or waiting for another process |
| `-14` | `EFAULT` | any call whose user pointer range is not fully accessible |
| `-22` | `EINVAL` | `send` to the caller's own process, oversized `write` |
| `-38` | `ENOSYS` | unknown call number |
| `-90` | `EMSGSIZE` | IPC length outside `1..=64`, or receiver buffer too small |

These are the Linux numbers for the same conditions. They are used because they
are familiar, not because Linux compatibility exists.

## Process startup state

The kernel loads a static ELF image and enters its entry point at EL0 with:

- `x0` = the process's first argument from the boot image table (the test
  processes use this as a peer pid; `init` and the fault test take none).
- `x1` = the second argument (zero in 0.0.6).
- `SP_EL0` = 48 bytes below the thread's user-stack top.
- `SPSR_EL1` = `0`: EL0t, AArch64, all interrupts unmasked.
- `ELR_EL1` = the ELF entry point.

The initial stack record is 48 zero bytes:

```text
u64 argc = 0
u64 argv[0] = NULL
u64 envp[0] = NULL
u64 AT_NULL.type = 0
u64 AT_NULL.value = 0
u64 padding
```

There is no `argv`, `envp`, auxiliary vector, TLS or dynamic linker in 0.0.6.

A process created with an extra thread starts the additional thread with the
same entry point, `x0 = 1` (its thread index), `x1 = 0`, its own kernel stack and
its own 16 KiB user stack. Threads share the process address space. This is a
fixed test convention, not a general `clone` interface.

## User address-space layout

| Range | Contents |
| --- | --- |
| `0x0000_0000_0000_1000` and above | Loaded ELF segments |
| `0x0000_0000_8000_0000` | Top of thread 0's user stack |
| `0x0000_0000_7FFF_C000` .. `0x0000_0000_8000_0000` | Thread 0's 16 KiB user stack |
| `0x0000_0000_7FFF_8000` .. `0x0000_0000_7FFF_C000` | Thread 1's 16 KiB user stack |

The stack grows down. Each thread stack is four pages and is mapped read/write,
non-executable, with `nG = 1` so it is tagged by the process ASID. Images are
linked at `0x0000_0000_0040_0000`; the loader does not require that base, only
that the image lies above page 0, ends below the lowest user stack and fits the
per-process page budget.

Executable pages are `PXN = 1`, `UXN = 0`; data and stack pages are `PXN = 1`,
`UXN = 1`. No page is ever mapped writable and executable.

## Limits

| Limit | Value |
| --- | --- |
| Processes | 8 (pid 1..7 usable) |
| Threads | 16 total |
| Threads per process | 2 |
| Pages per process | 64 (image plus stacks) |
| Message size | 64 bytes |
| `write` per call | 4096 bytes |
| Kernel stack per thread | 16 KiB, plus a 4 KiB guard page |
| User stack per thread | 16 KiB |
| Scheduling slice | 4 ms requested slice, weight 1024 (EEVDF) |
| Sleep | 10 seconds maximum per call |

`send` and `recv` have no timeout and are not interruptible: there is no signal
or cancellation mechanism in 0.0.6. A blocked thread waits until its peer acts
or dies. `sleep` waits for its own timer only.
