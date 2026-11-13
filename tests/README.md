# Tests (reserved)

This directory is reserved for cross-crate integration tests beyond the QEMU
smoke test.

Nothing is implemented here yet. Current tests are:

- `cargo test -p sketchos-driver-api`
- `tools/smoke-test.ps1` (normal and `-Framebuffer` QEMU boot validation)
