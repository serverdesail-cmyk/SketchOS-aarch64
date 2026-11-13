// SketchOS 0.0.6 wake-latency workload.
//
// The kernel gives this image two threads in one address space, both pinned to
// the boot core in this milestone. Thread 0 asks for a 2 ms sleep in a loop;
// thread 1 is a CPU hog with a long requested slice. EEVDF must preempt the hog
// when the sleeper's deadline arrives, and the kernel records each wake.
// `x0` is the thread index.
//
// `exit` reaps the whole process in this milestone, so the hog never exits: it
// acknowledges the sleeper and spins until the sleeper's `exit` reaps it. That
// guarantees both "done" lines are printed before the process disappears.

#![no_std]
#![no_main]

include!("runtime.rs");

use core::sync::atomic::{AtomicU32, Ordering};

const SLEEP_NS: u64 = 2_000_000;
const ROUNDS: u32 = 25;

static STOP: AtomicU32 = AtomicU32::new(0);
static ACK: AtomicU32 = AtomicU32::new(0);

#[no_mangle]
pub extern "C" fn _start(id: u64) -> ! {
    if id == 0 {
        let mut round = 0;
        while round < ROUNDS {
            sleep_ns(SLEEP_NS);
            round += 1;
        }
        STOP.store(1, Ordering::SeqCst);
        while ACK.load(Ordering::SeqCst) == 0 {
            yield_now();
        }
        write_str(STDOUT, "latency: short-slice sleeper done\n");
        exit(0)
    } else {
        let mut spins = 0u64;
        while STOP.load(Ordering::SeqCst) == 0 {
            spins = spins.wrapping_add(1);
        }
        if spins == 0 {
            write_str(STDERR, "latency: hog never observed the sleeper\n");
        }
        write_str(STDOUT, "latency: long-slice hog done\n");
        ACK.store(1, Ordering::SeqCst);
        loop {
            yield_now();
        }
    }
}
