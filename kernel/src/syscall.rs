//! SVC system-call dispatch for EL0.
//!
//! 0.0.5 fixes the ABI in `docs/syscalls.md`: the call number arrives in `x8`,
//! arguments in `x0..x3`, the result returns in `x0`, and every call traps with
//! `svc #0`. Errors are negative Linux-style errno values.
//!
//! The dispatcher runs on the faulting thread's kernel stack from
//! `exception_handler`. AArch64 records the address *after* `svc #0` in
//! `ELR_EL1`, so a normal return resumes at the next instruction without any
//! adjustment. `exit`, `yield`, a blocking `send`/`recv` and a faulting copy
//! return a *different* trap frame; the exception return path installs
//! whichever frame the dispatcher returns.

use crate::arch::aarch64::TrapFrame;
use crate::{console, process};

/// Return the syscall ABI version.
pub const SYS_ABI_VERSION: u64 = 0;
/// Terminate the calling thread and reap its process.
pub const SYS_EXIT: u64 = 1;
/// Write bytes from the calling process to a console descriptor.
pub const SYS_WRITE: u64 = 2;
/// Give up the rest of the current scheduling slice.
pub const SYS_YIELD: u64 = 3;
/// Send a bounded message to a process.
pub const SYS_SEND: u64 = 4;
/// Receive a bounded message from a process.
pub const SYS_RECV: u64 = 5;
/// Sleep for a nanosecond duration.
pub const SYS_SLEEP: u64 = 6;

/// `abi_version` result: major 1, minor 0.
pub const ABI_VERSION: i64 = 0x0001_0000;

const EBADF: i64 = -9;
const EFAULT: i64 = -14;
const EINVAL: i64 = -22;
const ENOSYS: i64 = -38;

/// Console descriptors accepted by `write`.
const STDOUT: u64 = 1;
const STDERR: u64 = 2;

/// Largest `write` this milestone accepts in one call.
const MAX_WRITE: u64 = 4096;
/// Copy buffer for user memory. Bounded so a hostile length cannot ask for a
/// large kernel allocation.
const CHUNK: usize = 64;

/// Dispatch one `svc #0`. Returns the frame to resume.
pub fn dispatch(frame: *mut TrapFrame) -> *mut TrapFrame {
    // SAFETY: `exception_handler` passes the live lower-EL trap frame it owns.
    let (number, arg0, arg1, arg2) = unsafe {
        let frame = &*frame;
        (frame.x[8], frame.x[0], frame.x[1], frame.x[2])
    };

    match number {
        SYS_ABI_VERSION => ret(frame, ABI_VERSION),
        SYS_EXIT => process::exit_current(frame, arg0),
        SYS_WRITE => write(frame, arg0, arg1, arg2),
        SYS_YIELD => process::yield_current(frame),
        SYS_SEND => process::send_current(frame, arg0, arg1, arg2),
        SYS_RECV => process::recv_current(frame, arg0, arg1, arg2),
        SYS_SLEEP => process::sleep_current(frame, arg0),
        _ => {
            crate::println!("Userspace: unknown syscall {}", number);
            ret(frame, ENOSYS)
        }
    }
}

fn ret(frame: *mut TrapFrame, value: i64) -> *mut TrapFrame {
    // SAFETY: the exception path owns the frame.
    unsafe {
        (*frame).x[0] = value as u64;
    }
    frame
}

fn write(frame: *mut TrapFrame, descriptor: u64, address: u64, length: u64) -> *mut TrapFrame {
    if descriptor != STDOUT && descriptor != STDERR {
        return ret(frame, EBADF);
    }
    if length == 0 {
        return ret(frame, 0);
    }
    if length > MAX_WRITE {
        return ret(frame, EINVAL);
    }

    let mut buffer = [0u8; CHUNK];
    let mut written = 0usize;
    let total = length as usize;
    while written < total {
        let chunk = (total - written).min(CHUNK);
        if process::copy_from_current_user(address + written as u64, &mut buffer[..chunk]).is_err()
        {
            return ret(frame, EFAULT);
        }
        console::write_bytes(&buffer[..chunk]);
        written += chunk;
    }
    ret(frame, written as i64)
}
