# Real-hardware bring-up

This document describes the gated procedure for booting SketchOS on the HP
OmniBook X. Nothing here has been executed on the physical machine yet. The
laptop remains unverified, and no physical boot may be performed without
explicit authorization.

## Safety rules

1. **Do not write to the internal disk.** The current milestone has no storage
   driver and no installer. All testing is from a removable USB medium.
2. **Do not change firmware settings without recording them.** Note the Secure
   Boot state, boot order and any TPM/device-encryption settings before and
   after.
3. **Do not modify `C:\SketchOS\hardware`.** It is read-only research input.
4. **Ask for explicit confirmation** before booting the laptop from the USB
   medium, and again before any action that could touch the internal disk.
5. **Have a recovery path.** Know how to enter the firmware boot menu and how to
   remove the USB medium before starting.
6. **Never point a raw-image writer at a fixed disk.** A raw image write erases
   the whole target device. Confirm the target is the removable USB medium by
   size, bus type and drive letter before writing.

## Prerequisites

- A USB flash drive that can be erased (at least 128 MiB free; the image is
  65 MiB).
- The laptop's firmware boot menu key (consult the HP manual for this model).
- Secure Boot state confirmed. The unsigned `BOOTAA64.EFI` loader will not run
  with Secure Boot enforcing unless it is signed or Secure Boot is disabled.
  Disabling Secure Boot is a firmware change and must be the user's decision.
- The built artifacts from `tools/build.ps1`:
  - `images/sketchos-esp.img` - the verified 65 MiB bootable MBR/FAT32 image;
  - `images/BOOTAA64.EFI` and `images/sketchos-kernel.elf` - the individual
    files used by the copy-only procedure.

`images/sketchos-esp.img` is built by `tools/make-esp-image.ps1`. The builder
verifies the MBR, the FAT32 geometry, both FAT chains and the SHA-256 of both
files after writing. QEMU also boots the image directly with
`tools/smoke-test.ps1 -EspImage`, so the artifact is observed bootable before it
is copied anywhere.

## Expected medium layout

The ESP image contains:

```text
<USB root>\
  EFI\
    BOOT\
      BOOTAA64.EFI
    SketchOS\
      kernel.elf
```

The medium must be FAT32. The loader is a UEFI application, not a second-stage
bootloader; UEFI firmware finds `\EFI\BOOT\BOOTAA64.EFI` on the removable
medium.

## Two ways to prepare the USB medium

### Option A: copy the files (no raw disk write)

This is the safer option and the one implemented by the guarded helper.

1. Format the USB medium as FAT32 (Windows Explorer or `format`).
2. Copy the `EFI` tree, or run:
   ```powershell
   powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\make-usb.ps1 -Drive E:
   ```
   The script requires an explicit drive letter, refuses system and fixed disks,
   and only formats when both `-Format` and `-ConfirmFormat` are supplied.
3. Confirm the drive letter is the USB medium, not an internal volume.

### Option B: write the image (erases the whole device)

Use this only when you specifically want to test the exact image artifact. It
requires an external raw-image writer; this repository does not ship an
unrestricted physical-disk writer. Typical choices are Rufus in "DD image" mode
or an equivalent tool.

1. Insert the USB medium and identify it unambiguously. Record its size and
   device path. Remove every other removable device if you are unsure.
2. In the imaging tool, select `images/sketchos-esp.img` as the source and the
   **removable USB device** as the destination. Do not select a hard disk, an
   internal SSD, a system partition or a virtual disk.
3. Verify the image hash before writing:
   ```powershell
   Get-FileHash .\images\sketchos-esp.img -Algorithm SHA256
   ```
   The 2026-11-12 release image is expected to be
   `d31c8f16ce81747f69f0ebcda88dc3856c854548e702fcca6546e9095f106cbe`.
4. Write the image. The tool will erase the entire target device.
5. Eject and re-insert the medium, then confirm it contains
   `EFI\BOOT\BOOTAA64.EFI` and `EFI\SketchOS\kernel.elf`.

## Serial console limitation

The laptop's SPCR describes interface type `0x13` at `0x894000`. That is a
Qualcomm/SDM845-style UART, not a PL011. The current kernel only drives a PL011
and will not print to this UART. Do not expect a serial log on the laptop.

Diagnostics therefore depend on:

- the loader's own GOP output before handoff, and
- the kernel's framebuffer text console, **if** the loader reports a GOP
  framebuffer for the panel.

If no framebuffer is reported, the machine may boot with no visible SketchOS
output. That is a known limitation, not a successful boot.

## Procedure

1. Build and prepare the medium with option A or B above.
2. Confirm the medium is the USB device, not an internal volume.
3. Power off the laptop.
4. Insert the USB medium and enter the firmware boot menu.
5. Select the USB device. Do not select an internal-disk entry.
6. Observe:
   - the loader banner and GOP framebuffer status,
   - the kernel banner and inventory,
   - the PPTT topology and cluster lines,
   - the MMU, exception, PCIe, GIC, timer, allocator and SMP lines,
   - the EEVDF scheduler lines: 8 per-core run queues, steals/migrations,
     one-shot timer events and idle disarms,
   - the user-phase lines, if the kernel reaches EL0 on the laptop,
   - `SKETCHOS 0.0.6 KERNEL OK` if the run completes.
7. The kernel powers off through PSCI only when it recognizes QEMU. On the
   laptop it halts instead, so a completed demo ends in a halted machine rather
   than a power-off. Hold the power button to turn it off.
8. Remove the USB medium and restore the original boot order.

## What to record

- Exact laptop model and BIOS version.
- Secure Boot state and whether it was changed.
- Whether the loader ran at all.
- Whether a GOP framebuffer was reported, and its resolution/format.
- Whether PPTT reported clusters and how many cores came online.
- Whether the EEVDF demo completed, and the observed steal/migration counts.
- The last visible line before a halt or reset.
- Any firmware error or beep.
- Whether the internal disk was visible or touched (it should not be).

## Known blockers

- No non-PL011 UART driver, so no serial diagnostics.
- No storage driver, so no installation or persistent logs.
- No GPU/panel driver; only the GOP framebuffer console, if firmware provides
  one.
- No SMP result on this laptop. Eight vCPUs are verified under QEMU; the
  laptop's eight-core configuration is untested.
- No recoverable exceptions on the laptop, so a fault halts with a register
  dump.
- EL2 entry is supported and QEMU-verified; the laptop entry level is unverified.
- The scheduler uses one global IRQ-safe lock; this is a functional kernel, not
  a validated real-time or scalable scheduler.

## Rollback

1. Power off.
2. Remove the USB medium.
3. Restore the firmware boot order and Secure Boot state recorded earlier.
4. Verify the internal operating system still boots.

No SketchOS code writes to the internal disk in this milestone. If a future
milestone adds an installer, this document must be updated with an explicit
confirmation gate and a recovery plan before it is used.
