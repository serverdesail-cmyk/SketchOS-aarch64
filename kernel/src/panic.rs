//! Kernel panic handler: report and park the core.

use crate::{arch, println};

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    // The panicking core may have been interrupted while holding the console
    // lock. Release it so the diagnostic is visible instead of spinning.
    crate::console::force_unlock();
    println!();
    println!("PANIC: {}", info);
    println!("SKETCHOS 0.0.4 KERNEL PANIC");
    arch::halt()
}
