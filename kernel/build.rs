//! Rebuild the kernel when the linker script changes.
//!
//! The script is passed through `RUSTFLAGS`, so Cargo does not track it on its
//! own. Without this hint a layout change would silently keep the previous
//! image until an unrelated source file was touched.

fn main() {
    println!("cargo:rerun-if-changed=linker/kernel.ld");
    println!("cargo:rerun-if-changed=../userspace/runtime.rs");
    println!("cargo:rerun-if-changed=../userspace/user.ld");
    for image in ["init.elf", "ipc-recv.elf", "ipc-send.elf", "faulter.elf", "threads.elf"] {
        println!("cargo:rerun-if-changed=../userspace/build/{image}");
    }
}
