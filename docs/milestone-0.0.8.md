# Milestone 0.0.8: physical boot diagnostics

**Status:** source committed; automated ISO build pending. This is a diagnostic
revision of the 0.0.7 persistent-shell branch, not a claim of successful
physical hardware boot.

## Version identification

The UEFI loader banner and the kernel banner both identify as **0.0.8**.
This deliberately distinguishes the diagnostic image from older 0.0.6 and
0.0.7 images.

## Boot-transition checkpoints

The loader writes progress directly to the UEFI GOP framebuffer, including
after `ExitBootServices`, when UEFI text output is no longer available:

1. `HANDOFF: PREPARING`
2. `HANDOFF: EXITING UEFI`
3. `HANDOFF: UEFI EXITED`
4. `HANDOFF: MEMORY MAP OK`
5. `HANDOFF: PAGE TABLES BUILT`
6. `HANDOFF: ENTERING KERNEL`

The first Rust kernel output is `KERNEL: entry reached`, followed by the
early framebuffer-console status. The normal kernel banner then reads
`SketchOS kernel 0.0.8 (AArch64)`.

The last visible line is diagnostic evidence:
- A last handoff checkpoint localizes the failure to the next loader or
  kernel-entry transition.
- `HANDOFF: ENTERING KERNEL` without `KERNEL: entry reached` means the
  failure is after the loader's final checkpoint, during the assembly entry,
  boot-info validation, exception setup, or early framebuffer initialization.
- `KERNEL: entry reached` confirms that the kernel reached its Rust entry
  point; subsequent lines narrow the failure further.

These checkpoints depend on a usable GOP framebuffer. If the firmware does
not expose one, the UEFI text output may remain the only visible output and
the absence of checkpoint text is inconclusive.

## Scope and limitations

This revision does not add a Qualcomm UART driver, USB HID, I2C-HID,
touchscreen, touchpad, ACPI power-button event handling, or GPU driver.
It does not make the laptop interactive. The shell still accepts input only
from a supported firmware-described PL011 UART.

## Verification

The GitHub Actions ISO workflow builds the AArch64 kernel and UEFI loader,
packages the EFI System Partition as an El Torito ISO, and publishes the ISO
and SHA-256 sidecar under `images/`. A successful CI build verifies
compilation and image structure only. Physical boot, framebuffer checkpoint
visibility, and the cause of any reboot remain unverified until tested on the
target laptop.
