# Milestone 0.0.7: persistent boot and explicit shutdown

**Status:** implementation staged on `milestone-0.0.7-persistent-shell`; not yet
built or verified. No physical-hardware support is claimed.

## Behavior change

The normal build now enables the small interactive shell by default. When the
embedded EL0 test workloads finish, the kernel reports their results and enters
the shell instead of automatically invoking the end-of-test power-off path.

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

Input is polling-based and currently reads only a firmware-described PL011
receive FIFO. The shell is not a USB keyboard driver and does not yet accept
input from the HP OmniBook X's Qualcomm UART.

If no supported PL011 input device is initialized, the kernel prints an
explicit diagnostic (through the framebuffer when available) and stays in the
persistent idle loop. It does not spin forever waiting for a nonexistent
serial input source.

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
5. Boot the target laptop from removable USB only after the image has passed
   QEMU checks. Confirm the kernel stays powered and accurately reports the
   lack of supported input; do not write to the internal disk.

No image produced from this branch should be called hardware-verified until
those tests have actually run.
