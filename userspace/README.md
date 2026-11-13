# Userspace

Status: minimal EL0 userspace implemented and verified in milestone 0.0.5. There
is no shell, filesystem, dynamic linker or system service yet.

The kernel embeds these images at build time and loads them into separate
address spaces. They run after the 0.0.4 kernel demo on the boot core.

## Contents

| File | Purpose |
| --- | --- |
| `runtime.rs` | Shared `no_std` runtime: syscall wrappers, `exit`, panic handler, `memcpy`/`memmove`/`memset`. Included by each program, not compiled separately. |
| `user.ld` | Linker script for a static AArch64 image based at `0x0040_0000`. |
| `init.rs` | Queries the ABI version, writes two lines, yields, exits 0. |
| `ipc_recv.rs` | Waits for `ping`, checks it, replies `pong`, exits. |
| `ipc_send.rs` | Sends `ping`, waits for `pong`, checks it, exits. |
| `faulter.rs` | Reads unmapped address `0x1000`; the kernel must terminate only this process. |
| `threads.rs` | Two threads in one process; a user-space barrier makes both print before exit. |
| `build/` | Generated ELF images. Not source; recreated by the build. |

The ABI is specified in [`docs/syscalls.md`](../docs/syscalls.md).

## Building

`tools/build.ps1` runs `tools/build-userspace.ps1` first, because the kernel
embeds the ELFs with `include_bytes!`. Building only the kernel without those
files fails.

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\build-userspace.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\build.ps1
```

Each program is a freestanding `aarch64-unknown-none` binary linked with
`user.ld`, `panic=abort`, a static relocation model and 4 KiB maximum page size.
The output is a non-PIE `ET_EXEC` image.

## Runtime model

- Entry is `_start`. The kernel enters at EL0 with `x0`/`x1` set from the spawn
  arguments and `SP_EL0` pointing at a 48-byte empty argument record.
- `write(1, ...)` and `write(2, ...)` reach the kernel console.
- `exit(status)` terminates the whole process; a panic calls `exit(101)`.
- `yield` gives up the rest of the slice.
- `send`/`recv` carry 1 to 64 bytes between processes with rendezvous semantics.
- There are no file descriptors, heap or threads API. Threads are created only
  by the kernel for the fixed test image, which receives the thread index in
  `x0`.

Each image is limited by the kernel to 64 mapped pages including its stacks.
`init`, the IPC images and the faulter use two pages each; `threads.elf` uses
three pages including `.bss`.

## Limits

- No `argv`, `envp`, auxiliary vector or TLS.
- No dynamic linking; `PT_DYNAMIC`, `PT_INTERP` and `PT_TLS` are rejected.
- No filesystem, sockets, signals, timers or process creation.
- No user-space allocator and no `std`.
- IPC is copy-based; shared memory is not implemented in 0.0.5.
- A blocked `send`/`recv` has no timeout.
