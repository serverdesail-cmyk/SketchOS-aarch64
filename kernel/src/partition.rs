//! Partition enumeration for the current block-device prototype.

use crate::storage::{self, SECTOR_SIZE};
use crate::sync::IrqSpinLock;

#[derive(Clone, Copy)]
pub struct Partition {
    pub index: u8,
    pub start_lba: u64,
    pub num_sectors: u64,
    pub type_code: u8,
    pub bootable: bool,
}

pub const MAX_PARTS: usize = 16;
const EMPTY_PARTITION: Partition = Partition {
    index: 0,
    start_lba: 0,
    num_sectors: 0,
    type_code: 0,
    bootable: false,
};

struct PartitionTable {
    entries: [Partition; MAX_PARTS],
    count: usize,
}

impl PartitionTable {
    const fn empty() -> Self {
        Self {
            entries: [EMPTY_PARTITION; MAX_PARTS],
            count: 0,
        }
    }

    fn push(&mut self, mut partition: Partition) {
        if self.count >= MAX_PARTS {
            return;
        }
        partition.index = self.count as u8;
        self.entries[self.count] = partition;
        self.count += 1;
    }
}

static PARTITIONS: IrqSpinLock<PartitionTable> = IrqSpinLock::new(PartitionTable::empty());

pub fn count() -> usize {
    PARTITIONS.lock().count
}

pub fn get(index: usize) -> Option<Partition> {
    let table = PARTITIONS.lock();
    if index < table.count {
        Some(table.entries[index])
    } else {
        None
    }
}

pub fn enumerate() {
    let mut found = PartitionTable::empty();
    let capacity = match storage::capacity_sectors() {
        Some(capacity) => capacity,
        None => {
            *PARTITIONS.lock() = found;
            crate::println!("Partition: no block device");
            return;
        }
    };

    let mut sector = [0u8; SECTOR_SIZE];
    if storage::read_sector(0, &mut sector).is_err() {
        *PARTITIONS.lock() = found;
        crate::println!("Partition: read error");
        return;
    }
    if sector[510] != 0x55 || sector[511] != 0xaa {
        *PARTITIONS.lock() = found;
        crate::println!("Partition: no MBR signature");
        return;
    }

    let protective_gpt = (0..4).any(|index| sector[0x1be + index * 16 + 4] == 0xee);
    if protective_gpt {
        crate::println!("Partition: GPT detected");
        let mut header = [0u8; SECTOR_SIZE];
        if storage::read_sector(1, &mut header).is_err() || &header[..8] != b"EFI PART" {
            *PARTITIONS.lock() = found;
            crate::println!("Partition: unreadable or invalid GPT header");
            return;
        }

        let header_size = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);
        if !(92..=SECTOR_SIZE as u32).contains(&header_size) {
            *PARTITIONS.lock() = found;
            crate::println!("Partition: invalid GPT header size");
            return;
        }
        let entries_lba = u64::from_le_bytes([
            header[72], header[73], header[74], header[75],
            header[76], header[77], header[78], header[79],
        ]);
        let num_entries = u32::from_le_bytes([header[80], header[81], header[82], header[83]]);
        let entry_size = u32::from_le_bytes([header[84], header[85], header[86], header[87]]);
        if entry_size < 128 || entry_size > SECTOR_SIZE as u32 || entry_size % 128 != 0 {
            *PARTITIONS.lock() = found;
            crate::println!("Partition: unsupported GPT entry size");
            return;
        }

        let entries_to_read = num_entries.min(MAX_PARTS as u32);
        let mut cached_lba = u64::MAX;
        let mut entries_sector = [0u8; SECTOR_SIZE];
        for index in 0..entries_to_read {
            let byte_offset = match (index as u64).checked_mul(entry_size as u64) {
                Some(value) => value,
                None => break,
            };
            let sector_offset = byte_offset / SECTOR_SIZE as u64;
            let within = (byte_offset % SECTOR_SIZE as u64) as usize;
            let lba = match entries_lba.checked_add(sector_offset) {
                Some(value) if value < capacity => value,
                _ => break,
            };
            if lba != cached_lba {
                if storage::read_sector(lba, &mut entries_sector).is_err() {
                    break;
                }
                cached_lba = lba;
            }

            let entry = &entries_sector[within..within + entry_size as usize];
            if entry[..16].iter().all(|&byte| byte == 0) {
                continue;
            }
            let start = u64::from_le_bytes([
                entry[32], entry[33], entry[34], entry[35],
                entry[36], entry[37], entry[38], entry[39],
            ]);
            let end = u64::from_le_bytes([
                entry[40], entry[41], entry[42], entry[43],
                entry[44], entry[45], entry[46], entry[47],
            ]);
            if end < start || end >= capacity {
                continue;
            }
            found.push(Partition {
                index: 0,
                start_lba: start,
                num_sectors: end - start + 1,
                type_code: 0,
                bootable: false,
            });
        }
        let count = found.count;
        *PARTITIONS.lock() = found;
        crate::println!("Partition: {} GPT partition(s) found", count);
        return;
    }

    // MBR: four primary partitions. Extended/logical partitions are not yet supported.
    for index in 0..4 {
        let offset = 0x1be + index * 16;
        let type_code = sector[offset + 4];
        if type_code == 0 {
            continue;
        }
        let bootable = sector[offset] == 0x80;
        let start = u32::from_le_bytes([
            sector[offset + 8],
            sector[offset + 9],
            sector[offset + 10],
            sector[offset + 11],
        ]) as u64;
        let count = u32::from_le_bytes([
            sector[offset + 12],
            sector[offset + 13],
            sector[offset + 14],
            sector[offset + 15],
        ]) as u64;
        if count == 0 || start.checked_add(count).map_or(true, |end| end > capacity) {
            continue;
        }
        found.push(Partition {
            index: 0,
            start_lba: start,
            num_sectors: count,
            type_code,
            bootable,
        });
    }

    let count = found.count;
    *PARTITIONS.lock() = found;
    crate::println!("Partition: {} MBR partition(s) found", count);
}

pub fn report() {
    let n = count();
    crate::println!("Partitions: {}", n);
    for index in 0..n {
        if let Some(partition) = get(index) {
            crate::println!(
                "  [{}] start={} sectors={} type=0x{:02x}{}",
                partition.index,
                partition.start_lba,
                partition.num_sectors,
                partition.type_code,
                if partition.bootable { " bootable" } else { "" }
            );
        }
    }
}
