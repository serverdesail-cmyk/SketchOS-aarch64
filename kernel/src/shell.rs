//! Interactive shell (milestone 0.0.7), opt-in via `--features shell`.
//!
//! Reads a line from the PL011 RX FIFO and dispatches a small command set.
//! Polling-based; no allocation, no blocking syscall. `poweroff` exits.

use crate::console;
use crate::ext2;
use crate::partition;
use crate::print;
use crate::println;
use crate::storage;
use crate::storage::BlockDevice;

pub fn run() -> ! {
    println!();
    println!("SketchOS shell. Type 'help'.");
    let mut line = [0u8; 128];
    let mut len = 0usize;
    loop {
        // prompt
        print!("sketchos> ");
        // read a line
        len = 0;
        loop {
            if let Some(b) = console::read_byte() {
                match b {
                    b'\r' | b'\n' => {
                        console::write_byte(b'\r');
                        console::write_byte(b'\n');
                        break;
                    }
                    0x08 | 0x7f => {
                        if len > 0 {
                            len -= 1;
                            // erase
                            console::write_byte(0x08);
                            console::write_byte(b' ');
                            console::write_byte(0x08);
                        }
                    }
                    b if b >= 0x20 && b < 0x7f => {
                        if len < line.len() - 1 {
                            line[len] = b;
                            len += 1;
                            console::write_byte(b);
                        }
                    }
                    _ => {}
                }
            } else {
                // idle
                core::hint::spin_loop();
            }
        }
        line[len] = 0;
        let cmd = core::str::from_utf8(&line[..len]).unwrap_or("");
        let cmd = cmd.trim();
        if cmd.is_empty() {
            continue;
        }
        let mut parts = cmd.split_whitespace();
        let name = parts.next().unwrap_or("");
        match name {
            "help" => {
                println!("commands: help version cpus mem storage poweroff");
            }
            "version" => {
                println!("SketchOS 0.0.7 (AArch64)");
            }
            "cpus" => {
                println!("online cores: {}", crate::percpu::records().len());
            }
            "mem" => {
                println!("phys allocator: see boot log");
            }
            "storage" => {
                if let Some(dev) = storage::device() {
                    println!("{}: {} sectors", dev.name(), dev.capacity_sectors());
                } else {
                    println!("no block device");
                }
            }
            "partitions" => {
                partition::enumerate();
                partition::report();
            }
            "mkfs" => {
                let mut args = parts;
                let part_idx: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                let fstype = args.next().unwrap_or("ext2");
                if let Some(p) = partition::get(part_idx) {
                    match fstype {
                        "ext2" => {
                            match ext2::format(p.start_lba, p.num_sectors) {
                                Ok(()) => println!("formatted partition {} as ext2", part_idx),
                                Err(e) => println!("mkfs failed: {}", e),
                            }
                        }
                        _ => println!("unknown fs: {}", fstype),
                    }
                } else {
                    println!("no such partition");
                }
            }
            "poweroff" => {
                println!("powering off");
                crate::process::finish();
            }
            _ => {
                println!("unknown command: {}", name);
            }
        }
    }
}