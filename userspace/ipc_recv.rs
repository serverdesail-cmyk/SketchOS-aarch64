// SketchOS 0.0.5 IPC receiver. x0 is the peer process id.

#![no_std]
#![no_main]

include!("runtime.rs");

#[no_mangle]
pub extern "C" fn _start(peer: u64) -> ! {
    let mut message = [0u8; MAX_MESSAGE];
    write_str(STDOUT, "ipc: receiver waiting\n");
    let length = recv(0, &mut message);
    if length <= 0 {
        write_str(STDERR, "ipc: receiver recv failed\n");
        exit(2);
    }
    let length = length as usize;
    if length != 4
        || message[0] != b'p'
        || message[1] != b'i'
        || message[2] != b'n'
        || message[3] != b'g'
    {
        write_str(STDERR, "ipc: receiver got the wrong message\n");
        exit(3);
    }
    write_str(STDOUT, "ipc: receiver got ping\n");
    if send(peer, b"pong") < 0 {
        write_str(STDERR, "ipc: receiver reply failed\n");
        exit(4);
    }
    write_str(STDOUT, "ipc: receiver sent pong\n");
    exit(0)
}
