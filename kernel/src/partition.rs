//! Partition enumeration (milestone 0.0.8).

use crate::storage::{self, BlockDevice, SECTOR_SIZE};

#[derive(Clone, Copy)]
pub struct Partition {
    pub index: u8,
    pub start_lba: u64,
    pub num_sectors: u64,
    pub type_code: u8,
    pub bootable: bool,
}

pub const MAX_PARTS: usize = 16;
static mut PARTS: [Partition; MAX_PARTS] = [Partition { index: 0, start_lba: 0, num_sectors: 0, type_code: 0, bootable: false }; MAX_PARTS];
static mut PART_COUNT: usize = 0;

pub fn count() -> usize {
    unsafe { PART_COUNT }
}

pub fn get(i: usize) -> Option<Partition> {
    unsafe {
        if i < PART_COUNT { Some(PARTS[i]) } else { None }
    }
}

pub fn enumerate() {
    unsafe { PART_COUNT = 0; }
    let dev = match storage::device() {
        Some(d) => d,
        None => { crate::println!("Partition: no block device"); return; }
    };
    let mut sec = [0u8; SECTOR_SIZE];
    if dev.read(0, &mut sec).is_err() {
        crate::println!("Partition: read error");
        return;
    }
    if sec[510] != 0x55 || sec[511] != 0xAA {
        crate::println!("Partition: no MBR signature");
        return;
    }
    // GPT protective MBR?
    let mut gpt = false;
    for i in 0..4 {
        if sec[0x1BE + i * 16 + 4] == 0xEE {
            gpt = true;
        }
    }
    if gpt {
        crate::println!("Partition: GPT detected");
        let mut hdr = [0u8; SECTOR_SIZE];
        if dev.read(1, &mut hdr).is_err() { return; }
        if &hdr[0..8] != b"EFI PART" {
            crate::println!("Partition: bad GPT signature");
            return;
        }
        let entries_start = u64::from_le_bytes([hdr[72], hdr[73], hdr[74], hdr[75], hdr[76], hdr[77], hdr[78], hdr[79]]);
        let num_entries = u32::from_le_bytes([hdr[80], hdr[81], hdr[82], hdr[83]]);
        let entry_size = u32::from_le_bytes([hdr[84], hdr[85], hdr[86], hdr[87]]);
        if entry_size == 0 { return; }
        let n = num_entries.min(MAX_PARTS as u32) as usize;
        let mut lba = entries_start;
        let mut off: usize = 0;
        for _ in 0..n {
            if off + 128 > SECTOR_SIZE {
                lba += 1;
                off = 0;
            }
            let mut esec = [0u8; SECTOR_SIZE];
            if dev.read(lba, &mut esec).is_err() { break; }
            let base = off;
            let mut empty = true;
            for b in 0..16 {
                if esec[base + b] != 0 { empty = false; break; }
            }
            if !empty {
                let start = u64::from_le_bytes([esec[base+32], esec[base+33], esec[base+34], esec[base+35], esec[base+36], esec[base+37], esec[base+38], esec[base+39]]);
                let end = u64::from_le_bytes([esec[base+40], esec[base+41], esec[base+42], esec[base+43], esec[base+44], esec[base+45], esec[base+46], esec[base+47]]);
                let num = end.saturating_sub(start).saturating_add(1);
                unsafe {
                    if PART_COUNT < MAX_PARTS {
                        PARTS[PART_COUNT] = Partition { index: PART_COUNT as u8, start_lba: start, num_sectors: num, type_code: 0, bootable: false };
                        PART_COUNT += 1;
                    }
                }
            }
            off += entry_size as usize;
        }
        crate::println!("Partition: {} GPT partition(s) found", unsafe { PART_COUNT });
        return;
    }

    // MBR: 4 primary partitions
    for i in 0..4 {
        let off = 0x1BE + i * 16;
        let type_code = sec[off + 4];
        if type_code == 0 { continue; }
        let bootable = sec[off] == 0x80;
        let start_lba = u32::from_le_bytes([sec[off + 8], sec[off + 9], sec[off + 10], sec[off + 11]]) as u64;
        let num_sectors = u32::from_le_bytes([sec[off + 12], sec[off + 13], sec[off + 14], sec[off + 15]]) as u64;
        unsafe {
            if PART_COUNT < MAX_PARTS {
                PARTS[PART_COUNT] = Partition { index: PART_COUNT as u8, start_lba, num_sectors, type_code, bootable };
                PART_COUNT += 1;
            }
        }
    }
    crate::println!("Partition: {} MBR partition(s) found", unsafe { PART_COUNT });
}

pub fn report() {
    let n = count();
    crate::println!("Partitions: {}", n);
    for i in 0..n {
        if let Some(p) = get(i) {
            crate::println!("  [{}] start={} sectors={} type=0x{:02x}{}", p.index, p.start_lba, p.num_sectors, p.type_code, if p.bootable { " bootable" } else { "" });
        }
    }
}
