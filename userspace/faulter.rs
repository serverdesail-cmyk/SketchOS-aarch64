// SketchOS 0.0.5 fault test: a user data abort must terminate only this
// process, then the kernel must continue with the remaining processes.

#![no_std]
#![no_main]

include!("runtime.rs");

#[no_mangle]
pub extern "C" fn _start() -> ! {
    write_str(STDOUT, "fault: reading an unmapped address\n");
    // SAFETY: this is deliberately invalid. Address 0x1000 is not mapped in
    // the process address space, so the load raises a lower-EL data abort.
    let value = unsafe { core::ptr::read_volatile(0x1000 as *const u64) };
    write_str(STDERR, "fault: the invalid read unexpectedly succeeded\n");
    exit(value as u64)
}
