// SketchOS 0.0.5 IPC sender. x0 is the peer process id.

#![no_std]
#![no_main]

include!("runtime.rs");

#[no_mangle]
pub extern "C" fn _start(peer: u64) -> ! {
    write_str(STDOUT, "ipc: sender sending ping\n");
    if send(peer, b"ping") < 0 {
        write_str(STDERR, "ipc: sender send failed\n");
        exit(5);
    }
    let mut reply = [0u8; MAX_MESSAGE];
    let length = recv(peer, &mut reply);
    if length <= 0 {
        write_str(STDERR, "ipc: sender recv failed\n");
        exit(6);
    }
    let length = length as usize;
    if length != 4
        || reply[0] != b'p'
        || reply[1] != b'o'
        || reply[2] != b'n'
        || reply[3] != b'g'
    {
        write_str(STDERR, "ipc: sender got the wrong reply\n");
        exit(7);
    }
    write_str(STDOUT, "ipc: sender got pong\n");
    exit(0)
}
