# Milestone 0.0.9: earliest kernel-entry diagnostics

**Status:** source committed; automated ISO build pending. This diagnostic revision follows the 0.0.8 image, which reached the final handoff checkpoint on the target laptop but did not display the Rust kernel banner.

## Version identification

Both the UEFI loader and kernel identify as **0.0.9**.

## Assembly framebuffer markers

The AArch64 entry assembly paints three 16x16 GOP framebuffer squares:

- Green at x=16: _start has begun, before changing translation state.
- Blue at x=40: execution reached EL1 with the loader's translation tables active.
- Red at x=64: execution reached the higher-half continuation with the higher-half stack installed.

The markers are painted using the framebuffer's physical address before the translation switch, then through the linear physical-map alias after the switch. They do not depend on Rust, a UART driver, or the kernel text console.

## Rust entry and validation

The kernel now attempts to initialize the framebuffer console from the loader-provided boot record and prints `KERNEL: Rust entry reached` before strict boot-info validation. If validation fails after the console starts, the kernel prints `KERNEL: boot-info validation failed: ...` rather than silently halting. The ordinary kernel banner is now `SketchOS kernel 0.0.9 (AArch64)`.

## Reading the result

- No green square: the kernel entry stub was not reached, or the firmware framebuffer was not usable for the marker.
- Green only: execution reached _start but did not reach the EL1 entry point.
- Green and blue: EL1 translation activation completed; failure is later in the entry path.
- Green, blue and red: the higher-half continuation was reached. If Rust text is still absent, investigate the call into kmain, boot-record access, and framebuffer-console initialization.
- `KERNEL: Rust entry reached`: execution reached Rust. Any following boot-info validation message identifies the next issue.

Marker absence is not definitive if the firmware does not expose a writable linear GOP framebuffer or uses an unexpected pixel format.

## Scope and verification

This revision does not add Qualcomm UART, USB HID, I2C-HID, touchscreen, touchpad, ACPI power-button, or GPU drivers. The shell remains PL011-only. A successful CI build validates compilation and ISO structure, not physical hardware behavior. The HP OmniBook X 14 must be retested with this image.
