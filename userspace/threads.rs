// SketchOS 0.0.5 two-thread process.
//
// The kernel gives this image two threads that share one address space but run
// on separate kernel stacks and separate user stacks. `x0` is the thread index.
// Both threads meet at a barrier after printing; the barrier only releases once
// both have printed, so the output proves both threads ran before the process
// is reaped.

#![no_std]
#![no_main]

include!("runtime.rs");

use core::sync::atomic::{AtomicU32, Ordering};

static ONLINE: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicU32 = AtomicU32::new(0);

#[no_mangle]
pub extern "C" fn _start(id: u64) -> ! {
    if id == 0 {
        write_str(STDOUT, "threads: A online\n");
    } else {
        write_str(STDOUT, "threads: B online\n");
    }

    ONLINE.fetch_add(1, Ordering::SeqCst);
    while ONLINE.load(Ordering::SeqCst) < 2 {
        yield_now();
    }

    if id == 0 {
        write_str(STDOUT, "threads: A done\n");
    } else {
        write_str(STDOUT, "threads: B done\n");
    }

    DONE.fetch_add(1, Ordering::SeqCst);
    while DONE.load(Ordering::SeqCst) < 2 {
        yield_now();
    }
    exit(0)
}
