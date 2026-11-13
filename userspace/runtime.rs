// Shared SketchOS 0.0.6 user-side runtime.
//
// Each user program is a single no_std source file that starts with
// `include!("runtime.rs")`. The syscall ABI is fixed by docs/syscalls.md:
// x8 carries the call number, x0..x3 the arguments, x0 the result, and
// `svc #0` traps to EL1.

use core::arch::asm;

pub const SYS_ABI_VERSION: u64 = 0;
pub const SYS_EXIT: u64 = 1;
pub const SYS_WRITE: u64 = 2;
pub const SYS_YIELD: u64 = 3;
pub const SYS_SEND: u64 = 4;
pub const SYS_RECV: u64 = 5;
pub const SYS_SLEEP: u64 = 6;

pub const ABI_VERSION: u64 = 0x0001_0000;
pub const MAX_MESSAGE: usize = 64;

pub const STDIN: u64 = 0;
pub const STDOUT: u64 = 1;
pub const STDERR: u64 = 2;

#[inline(always)]
unsafe fn syscall0(number: u64) -> i64 {
    let result: i64;
    // SAFETY: the caller invokes a syscall the kernel documents.
    unsafe {
        asm!(
            "svc #0",
            in("x8") number,
            inlateout("x0") 0u64 => result,
            options(nostack),
        );
    }
    result
}

#[inline(always)]
unsafe fn syscall1(number: u64, arg0: u64) -> i64 {
    let result: i64;
    // SAFETY: see `syscall0`.
    unsafe {
        asm!(
            "svc #0",
            in("x8") number,
            inlateout("x0") arg0 => result,
            options(nostack),
        );
    }
    result
}

#[inline(always)]
unsafe fn syscall3(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    let result: i64;
    // SAFETY: see `syscall0`.
    unsafe {
        asm!(
            "svc #0",
            in("x8") number,
            inlateout("x0") arg0 => result,
            in("x1") arg1,
            in("x2") arg2,
            options(nostack),
        );
    }
    result
}

pub fn abi_version() -> u64 {
    // SAFETY: call 0 is the read-only ABI query.
    unsafe { syscall0(SYS_ABI_VERSION) as u64 }
}

pub fn write(fd: u64, bytes: &[u8]) -> i64 {
    // SAFETY: `bytes` remains alive for the duration of the call.
    unsafe { syscall3(SYS_WRITE, fd, bytes.as_ptr() as u64, bytes.len() as u64) }
}

pub fn write_str(fd: u64, text: &str) -> i64 {
    write(fd, text.as_bytes())
}

pub fn yield_now() {
    // SAFETY: yield takes no arguments and has no failure mode.
    unsafe {
        syscall0(SYS_YIELD);
    }
}

pub fn send(pid: u64, bytes: &[u8]) -> i64 {
    if bytes.len() > MAX_MESSAGE {
        return -22;
    }
    // SAFETY: the kernel copies the buffer before returning.
    unsafe { syscall3(SYS_SEND, pid, bytes.as_ptr() as u64, bytes.len() as u64) }
}

pub fn recv(pid: u64, buffer: &mut [u8]) -> i64 {
    if buffer.is_empty() {
        return -22;
    }
    let capacity = buffer.len().min(MAX_MESSAGE);
    // SAFETY: the kernel writes at most `capacity` bytes into the buffer.
    unsafe { syscall3(SYS_RECV, pid, buffer.as_mut_ptr() as u64, capacity as u64) }
}

pub fn sleep_ns(duration_ns: u64) -> i64 {
    // SAFETY: sleep takes one scalar argument and no pointers.
    unsafe { syscall1(SYS_SLEEP, duration_ns) }
}

pub fn exit(code: u64) -> ! {
    // SAFETY: exit never returns to this thread.
    unsafe {
        syscall1(SYS_EXIT, code);
    }
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    exit(101)
}

// Compiler-generated helpers: a freestanding image must provide its own.
#[no_mangle]
pub unsafe extern "C" fn memcpy(
    destination: *mut core::ffi::c_void,
    source: *const core::ffi::c_void,
    count: usize,
) -> *mut core::ffi::c_void {
    let destination = destination as *mut u8;
    let source = source as *const u8;
    let mut index = 0;
    while index < count {
        // SAFETY: the caller contract for `memcpy` requires valid ranges.
        unsafe { *destination.add(index) = *source.add(index) };
        index += 1;
    }
    destination as *mut core::ffi::c_void
}

#[no_mangle]
pub unsafe extern "C" fn memmove(
    destination: *mut core::ffi::c_void,
    source: *const core::ffi::c_void,
    count: usize,
) -> *mut core::ffi::c_void {
    if (destination as usize) <= source as usize {
        // SAFETY: forwarded `memmove` contract.
        return unsafe { memcpy(destination, source, count) };
    }
    let destination = destination as *mut u8;
    let source = source as *const u8;
    let mut index = count;
    while index > 0 {
        index -= 1;
        // SAFETY: see `memcpy`; the backward copy handles overlap.
        unsafe { *destination.add(index) = *source.add(index) };
    }
    destination as *mut core::ffi::c_void
}

#[no_mangle]
pub unsafe extern "C" fn memset(
    destination: *mut core::ffi::c_void,
    value: i32,
    count: usize,
) -> *mut core::ffi::c_void {
    let destination = destination as *mut u8;
    let mut index = 0;
    while index < count {
        // SAFETY: the caller contract for `memset` requires a valid range.
        unsafe { *destination.add(index) = value as u8 };
        index += 1;
    }
    destination as *mut core::ffi::c_void
}
