# Milestone 0.0.7: persistent boot and explicit shutdown

**Status:** boot flow reorganized on `milestone-0.0.7-persistent-shell`; CI ISO build
is triggered by source changes. Physical-hardware and interactive-input support
are not verified.

## Behavior change

The normal build enables the small interactive shell by default and now enters
it immediately after the minimum core initialization sequence (boot-info
validation, exception vectors, MMU/framebuffer console, ACPI platform copy,
physical allocator, heap, and boot-CPU/PSCI discovery).

The normal shell build deliberately skips optional storage/network/DRM probing,
secondary-CPU startup, the scheduler benchmark, and embedded EL0 stress tests.
Those are valuable diagnostics, but they should not stand between firmware
handoff and the first prompt. The non-shell build retains the full diagnostic
and scheduler workload.

A build without the shell and without the smoke-test feature enters an
interruptible `WFI` idle loop. It does not request PSCI `SYSTEM_OFF`.

The automated QEMU smoke test has a separate `smoke-test` feature. Only that
dedicated build retains automatic QEMU shutdown so the test runner can detect
completion. Fatal pre-scheduler and userspace-setup paths are similarly gated
to the smoke-test feature.

## Shell

The shell currently supports:

- `help`
- `version`
- `cpus`
- `mem`
- `storage`
- `partitions`
- `mkfs`
- `poweroff`

Storage and partition commands report that probing is deferred in this
early-shell profile; they do not touch storage hardware during boot. The
full diagnostic profile retains the existing device-probe sequence.

Input is polling-based and currently reads only a firmware-described PL011
receive FIFO. The shell is not a USB keyboard driver and does not yet accept
input from the HP OmniBook X's Qualcomm UART.

If no supported PL011 input device is initialized, the kernel prints an
explicit diagnostic (through the framebuffer when available) and stays in the
persistent idle loop. It does not spin forever waiting for a nonexistent
serial input source. On the HP OmniBook X this is still the expected outcome
until the Qualcomm UART or a real HID transport is implemented; the early-shell
path makes this limitation visible immediately rather than after the scheduler
and test workload.

## Explicit shutdown

The `poweroff` command calls a dedicated explicit-shutdown path. That path
uses the SMC/HVC conduit declared by ACPI FADT only when the firmware declares
PSCI compliance. If PSCI is unavailable or rejects the request, the kernel
halts and tells the user to use the hardware power button.

The automatic smoke-test power-off helper remains QEMU-only and is compiled
into the dedicated `smoke-test` build path.

## Build commands

From the repository root:

```powershell
# Normal interactive build (PL011 serial shell when available)
powershell -NoProfile -ExecutionPolicy Bypass -File .\\tools\\build.ps1

# Optional kernel without shell; still stays powered after the test workload
powershell -NoProfile -ExecutionPolicy Bypass -File .\\tools\\build.ps1 -NoShell

# Dedicated QEMU test build (automatic power-off only for smoke tests)
powershell -NoProfile -ExecutionPolicy Bypass -File .\\tools\\smoke-test.ps1 -TimeoutSeconds 180
```

## Hardware discovery and input limitations

PCIe inventory now classifies USB host-controller programming interfaces, including xHCI (interface 0x30), EHCI, OHCI and UHCI. This identifies a host controller only; it does not enumerate USB ports, read USB device descriptors, parse HID report descriptors, or deliver keyboard reports. No USB HID keyboard is claimed to work yet.

The target laptop may expose its built-in keyboard and touchpad through an ACPI-described I2C-HID path rather than a USB HID interface. That requires AML namespace/device-resource discovery, the relevant I2C controller driver, GPIO/interrupt support and the HID-over-I2C transport. Those pieces are not implemented. The next input milestone must identify the actual ACPI and PCI topology from this laptop before selecting the transport.

## ISO packaging

`tools/make-iso.ps1` packages the FAT32 EFI System Partition image as an AArch64 UEFI El Torito boot image. It requires `xorriso` on PATH and writes `images/SketchOS-AArch64.iso`. The `Build AArch64 ISO` GitHub Actions workflow builds the pinned Rust targets on Ubuntu, creates the ISO, and commits the generated ISO into this branch. The workflow does not flash any physical drive.

## Hardware limitations

This milestone does **not** implement:

- USB HID keyboard support.
- A driver for the Qualcomm UART described by this laptop's SPCR.
- ACPI power-button event handling (GPE/AML/EC path).
- Native panel, GPU, or touchpad drivers.

Consequently, on the target laptop this milestone can keep the kernel alive and
show diagnostics through GOP if firmware exposes a framebuffer, but it cannot
yet provide an interactive keyboard shell or react to a short ACPI power-button
event. Those require separately verified hardware/firmware drivers. A long
hardware power-button hold remains the fallback to force the laptop off.

## Verification still required

Before this milestone is considered complete:

1. Build the default shell-enabled image with the pinned Rust toolchain.
2. Run host tests for the scheduler, ELF parser, memory manager and driver API.
3. Run the existing QEMU smoke-test matrix and confirm it still powers off only
   in the dedicated smoke-test build.
4. Run QEMU with a PL011 serial console connected interactively and verify
   command parsing plus `poweroff`.
5. Boot the target laptop from removable USB and record the final boot-stage
   message. Confirm it remains powered; do not write to the internal disk.

No image produced from this branch should be called hardware-verified until
those tests have actually run.
