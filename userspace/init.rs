// SketchOS 0.0.5 init process: exercise the ABI query, write, yield and exit.

#![no_std]
#![no_main]

include!("runtime.rs");

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let version = abi_version();
    if version != ABI_VERSION {
        write_str(STDERR, "init: unexpected syscall ABI version\n");
        exit(1);
    }
    write_str(STDOUT, "init: hello from EL0\n");
    write_str(STDOUT, "init: syscall ABI 0x00010000\n");
    yield_now();
    write_str(STDOUT, "init: exiting with status 0\n");
    exit(0)
}
