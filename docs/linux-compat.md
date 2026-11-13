# Linux compatibility layer — design (milestone 0.0.8 prerequisite)

SketchOS 0.0.8+ aims to run unmodified Linux userspace (including Wayland
compositors such as Hyprland, and Qualcomm WiFi drivers such as `ath12k`).
That is **not** a driver porting task: it requires a Linux-compatible
userspace ABI, a dynamic linker, and a kernel ABI shim. This document records
the architecture and the honest gaps. No code is added for it in 0.0.8.

## Why this is a separate track
Hyprland, Mesa/freedreno, libinput, wpa_supplicant and the Qualcomm WiFi
stack are Linux programs. They cannot be recompiled against SketchOS's native
ABI without a porting effort larger than the OS itself. The only realistic
path to "run Hyprland" is to make SketchOS **execute Linux ELF binaries**
against a Linux-compatible syscall surface.

## Layers required

1. **ELF interpreter / dynamic linker.** Linux AArch64 uses `/lib/ld-linux-aarch64.so.1`
   (or musl's `ld-musl-aarch64.so.1`). SketchOS must load PT_INTERP, map the
   interpreter, and start the process at its entry with the right stack layout
   (argc/argv/envp/auxv). The existing ELF loader (`sketchos-elf`) handles
   static PIEs only; it must be extended for ET_DYN with an interpreter.

2. **Syscall ABI shim.** Linux AArch64 uses `svc #0` with `x8` = syscall number.
   SketchOS uses its own `svc` ABI (`SYS_SLEEP = 6`, etc.). A shim must
   translate Linux syscall numbers to SketchOS implementations (or to a new
   Linux-compat syscall table). High-traffic syscalls (read/write/mmap/ioctl/
   epoll) must be fast paths.

3. **VFS + device nodes.** Linux userspace expects `/dev/dri/card0`,
   `/dev/input/event*`, `/dev/null`, `/dev/urandom`, etc. SketchOS needs a VFS
   with device-node semantics and ioctl dispatch.

4. **DRM/KMS subsystem (kernel).** `0.0.8` starts this: `kernel/src/drm.rs`
   provides a device, dumb buffers (GEM-style, unified-memory pages) and a
   modeset record. Still missing: planes, CRTCs, encoders, connectors, atomic
   commits, fences, and a real GPU driver.

5. **Input subsystem.** evdev-compatible event devices; libinput depends on it.

6. **Network stack.** TCP/IP, UDP, DNS, and a WiFi MAC/PHY. The Qualcomm
   FastConnect 7800 (WCN7850) has no bare-metal driver; Linux uses `ath12k`
   over cfg80211/mac80211. Porting it means reimplementing those Linux
   wireless subsystems, or running the Linux driver via a compatibility shim.

7. **Graphics stack.** Mesa (freedreno) needs DRM + GBM + EGL. Hyprland needs
   wlroots + wayland-protocols. These are userspace libraries that can be
   cross-compiled for AArch64 Linux and run under the compat layer.

## What 0.0.8 actually delivers
- `kernel/src/drm.rs`: DRM device, mode record, dumb-buffer allocation (phys
  pages, zeroed, 64-byte-aligned pitch). `DRM: mode 1920x1080@60` printed at
  boot.
- This document.

## What 0.0.8 does NOT deliver
- Hyprland, Wayland, Mesa, or any compositor.
- A real GPU driver (Adreno/freedreno).
- WiFi, networking, or input.
- The Linux-compat layer itself (design only).

## Optimization principles carried forward
- Unified memory: dumb buffers are ordinary physical pages shared between CPU
  and display controller; no staging copy (XNU IOSurface model).
- Cache-line-aware descriptors (128 B on Apple silicon; 64 B on Oryon).
- No per-buffer heap metadata beyond a fixed-capacity table.
- Zero-copy I/O paths (from 0.0.7 storage work).

## Honest assessment
Reaching "Hyprland on SketchOS" is a multi-year effort for a team, not a
session task. The 0.0.8 milestone is intentionally a *foundation* milestone:
it puts the DRM/KMS and Linux-compat architecture in place so future work has
a target. Anyone claiming Hyprland runs today would be lying.