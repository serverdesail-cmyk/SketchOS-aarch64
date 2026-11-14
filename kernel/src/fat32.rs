//! Read-only FAT32 volume reader for the current block-device prototype.
//!
//! Writes are deliberately disabled until journaling/metadata ordering and the
//! block-device DMA/cache contract are implemented.

use crate::storage::{self, SECTOR_SIZE};

const FAT32_EOC: u32 = 0x0fff_fff8;
const FAT32_BAD: u32 = 0x0fff_fff7;

pub struct Fat32 {
    partition_lba: u64,
    total_sectors: u64,
    sectors_per_cluster: u32,
    fat_start_lba: u64,
    fat_size_sectors: u32,
    data_start_lba: u64,
    root_cluster: u32,
    cluster_count: u32,
}

impl Fat32 {
    pub fn mount() -> Result<Self, &'static str> {
        let capacity = storage::capacity_sectors().ok_or("no block device")?;
        let mut sector0 = [0u8; SECTOR_SIZE];
        storage::read_sector(0, &mut sector0).map_err(|_| "read MBR")?;

        let partition_lba = if sector0[510] == 0x55 && sector0[511] == 0xaa {
            let start = u32::from_le_bytes([
                sector0[0x1be + 8],
                sector0[0x1be + 9],
                sector0[0x1be + 10],
                sector0[0x1be + 11],
            ]) as u64;
            start
        } else {
            0
        };
        if partition_lba >= capacity {
            return Err("partition starts beyond block device");
        }

        let mut bpb = [0u8; SECTOR_SIZE];
        storage::read_sector(partition_lba, &mut bpb).map_err(|_| "read BPB")?;
        if bpb[510] != 0x55 || bpb[511] != 0xaa {
            return Err("invalid FAT boot-sector signature");
        }

        let bytes_per_sector = u16::from_le_bytes([bpb[0x0b], bpb[0x0c]]) as u32;
        let sectors_per_cluster = bpb[0x0d] as u32;
        let reserved_sectors = u16::from_le_bytes([bpb[0x0e], bpb[0x0f]]) as u32;
        let num_fats = bpb[0x10] as u32;
        let total16 = u16::from_le_bytes([bpb[0x13], bpb[0x14]]) as u64;
        let fat_size16 = u16::from_le_bytes([bpb[0x16], bpb[0x17]]) as u32;
        let total32 = u32::from_le_bytes([bpb[0x20], bpb[0x21], bpb[0x22], bpb[0x23]]) as u64;
        let fat_size32 = u32::from_le_bytes([bpb[0x24], bpb[0x25], bpb[0x26], bpb[0x27]]);
        let root_cluster = u32::from_le_bytes([bpb[0x2c], bpb[0x2d], bpb[0x2e], bpb[0x2f]]);

        if bytes_per_sector != SECTOR_SIZE as u32 {
            return Err("unsupported bytes per sector");
        }
        if sectors_per_cluster == 0
            || sectors_per_cluster > 128
            || !sectors_per_cluster.is_power_of_two()
        {
            return Err("invalid sectors per cluster");
        }
        if reserved_sectors == 0 || num_fats == 0 || num_fats > 2 {
            return Err("invalid FAT layout");
        }

        let total_sectors = if total32 != 0 { total32 } else { total16 };
        let fat_size_sectors = if fat_size32 != 0 { fat_size32 } else { fat_size16 };
        if total_sectors == 0 || fat_size_sectors == 0 {
            return Err("not a FAT32 volume");
        }
        let volume_end = partition_lba
            .checked_add(total_sectors)
            .ok_or("volume size overflow")?;
        if volume_end > capacity {
            return Err("FAT volume exceeds block device");
        }

        let fat_start_lba = partition_lba
            .checked_add(reserved_sectors as u64)
            .ok_or("FAT start overflow")?;
        let all_fats_sectors = (num_fats as u64)
            .checked_mul(fat_size_sectors as u64)
            .ok_or("FAT size overflow")?;
        let data_start_lba = fat_start_lba
            .checked_add(all_fats_sectors)
            .ok_or("data start overflow")?;
        if data_start_lba >= volume_end {
            return Err("FAT metadata consumes the entire volume");
        }

        let data_sectors = volume_end - data_start_lba;
        let cluster_count_64 = data_sectors / sectors_per_cluster as u64;
        if cluster_count_64 == 0 || cluster_count_64 > 0x0fff_ffef {
            return Err("FAT32 cluster count is outside the supported range");
        }
        let cluster_count = cluster_count_64 as u32;
        if root_cluster < 2 || root_cluster - 2 >= cluster_count {
            return Err("root cluster is outside the data region");
        }

        Ok(Self {
            partition_lba,
            total_sectors,
            sectors_per_cluster,
            fat_start_lba,
            fat_size_sectors,
            data_start_lba,
            root_cluster,
            cluster_count,
        })
    }

    fn cluster_to_lba(&self, cluster: u32) -> Result<u64, &'static str> {
        if cluster < 2 || cluster - 2 >= self.cluster_count {
            return Err("cluster outside data region");
        }
        let offset = (cluster as u64 - 2)
            .checked_mul(self.sectors_per_cluster as u64)
            .ok_or("cluster offset overflow")?;
        let lba = self.data_start_lba.checked_add(offset).ok_or("cluster LBA overflow")?;
        let end = lba
            .checked_add(self.sectors_per_cluster as u64)
            .ok_or("cluster end overflow")?;
        let volume_end = self.partition_lba + self.total_sectors;
        if end > volume_end {
            return Err("cluster exceeds volume");
        }
        Ok(lba)
    }

    fn read_fat_entry(&self, cluster: u32) -> Result<u32, &'static str> {
        let fat_offset = cluster.checked_mul(4).ok_or("FAT offset overflow")?;
        let fat_sector_offset = (fat_offset / SECTOR_SIZE as u32) as u64;
        if fat_sector_offset >= self.fat_size_sectors as u64 {
            return Err("FAT entry outside FAT");
        }
        let lba = self.fat_start_lba + fat_sector_offset;
        let offset = (fat_offset % SECTOR_SIZE as u32) as usize;
        let mut sector = [0u8; SECTOR_SIZE];
        storage::read_sector(lba, &mut sector).map_err(|_| "read FAT sector")?;
        Ok(u32::from_le_bytes([
            sector[offset],
            sector[offset + 1],
            sector[offset + 2],
            sector[offset + 3],
        ]) & 0x0fff_ffff)
    }

    fn next_cluster(&self, cluster: u32) -> Result<Option<u32>, &'static str> {
        let next = self.read_fat_entry(cluster)?;
        if next >= FAT32_EOC {
            Ok(None)
        } else if next == FAT32_BAD || next < 2 || next - 2 >= self.cluster_count {
            Err("invalid FAT cluster chain")
        } else {
            Ok(Some(next))
        }
    }

    pub fn list_root(&self) -> Result<(), &'static str> {
        let mut cluster = self.root_cluster;
        let mut steps = 0u32;
        let mut sector = [0u8; SECTOR_SIZE];

        while steps < self.cluster_count {
            let first_lba = self.cluster_to_lba(cluster)?;
            for index in 0..self.sectors_per_cluster {
                storage::read_sector(first_lba + index as u64, &mut sector)
                    .map_err(|_| "read root directory")?;
                for entry in sector.chunks_exact(32) {
                    if entry[0] == 0 {
                        return Ok(());
                    }
                    if entry[0] == 0xe5 || entry[0x0b] == 0x0f || entry[0x0b] & 0x08 != 0 {
                        continue;
                    }
                    let (name, name_len) = short_name(entry);
                    let name = core::str::from_utf8(&name[..name_len]).unwrap_or("?");
                    let size = u32::from_le_bytes([entry[0x1c], entry[0x1d], entry[0x1e], entry[0x1f]]);
                    let is_dir = entry[0x0b] & 0x10 != 0;
                    crate::println!(
                        "  {}  {} bytes{}",
                        name,
                        size,
                        if is_dir { "  <dir>" } else { "" }
                    );
                }
            }
            match self.next_cluster(cluster)? {
                Some(next) => cluster = next,
                None => return Ok(()),
            }
            steps += 1;
        }
        Err("root directory chain exceeded cluster count")
    }

    /// Read a short-name file from the root directory. Returns the bytes copied;
    /// if the caller's buffer is smaller than the file, the result is truncated.
    pub fn read_file(&self, name: &str, buffer: &mut [u8]) -> Result<usize, &'static str> {
        let wanted = encode_short_name(name)?;
        let mut cluster = self.root_cluster;
        let mut steps = 0u32;
        let mut sector = [0u8; SECTOR_SIZE];
        let mut file_cluster = None;
        let mut file_size = 0u32;

        'search: while steps < self.cluster_count {
            let first_lba = self.cluster_to_lba(cluster)?;
            for index in 0..self.sectors_per_cluster {
                storage::read_sector(first_lba + index as u64, &mut sector)
                    .map_err(|_| "read root directory")?;
                for entry in sector.chunks_exact(32) {
                    if entry[0] == 0 {
                        break 'search;
                    }
                    if entry[0] == 0xe5 || entry[0x0b] == 0x0f || entry[0x0b] & 0x08 != 0 {
                        continue;
                    }
                    if entry[..11] == wanted {
                        let high = u16::from_le_bytes([entry[0x14], entry[0x15]]) as u32;
                        let low = u16::from_le_bytes([entry[0x1a], entry[0x1b]]) as u32;
                        file_cluster = Some((high << 16) | low);
                        file_size = u32::from_le_bytes([entry[0x1c], entry[0x1d], entry[0x1e], entry[0x1f]]);
                        break 'search;
                    }
                }
            }
            match self.next_cluster(cluster)? {
                Some(next) => cluster = next,
                None => break,
            }
            steps += 1;
        }

        let mut cluster = file_cluster.ok_or("file not found")?;
        let mut remaining = (file_size as usize).min(buffer.len());
        let mut copied = 0usize;
        let mut chain_steps = 0u32;

        while remaining > 0 && chain_steps < self.cluster_count {
            let first_lba = self.cluster_to_lba(cluster)?;
            for index in 0..self.sectors_per_cluster {
                if remaining == 0 {
                    break;
                }
                storage::read_sector(first_lba + index as u64, &mut sector)
                    .map_err(|_| "read file data")?;
                let count = remaining.min(SECTOR_SIZE);
                buffer[copied..copied + count].copy_from_slice(&sector[..count]);
                copied += count;
                remaining -= count;
            }
            if remaining == 0 {
                break;
            }
            cluster = self.next_cluster(cluster)?.ok_or("file chain ended early")?;
            chain_steps += 1;
        }

        if remaining != 0 {
            return Err("file chain exceeded cluster count");
        }
        Ok(copied)
    }
}

fn short_name(entry: &[u8]) -> ([u8; 13], usize) {
    let mut output = [0u8; 13];
    let mut length = 0usize;
    for &byte in &entry[..8] {
        if byte == b' ' { break; }
        output[length] = byte;
        length += 1;
    }
    if entry[8..11].iter().any(|&byte| byte != b' ') {
        output[length] = b'.';
        length += 1;
        for &byte in &entry[8..11] {
            if byte == b' ' { break; }
            output[length] = byte;
            length += 1;
        }
    }
    (output, length)
}

fn encode_short_name(name: &str) -> Result<[u8; 11], &'static str> {
    let bytes = name.as_bytes();
    let dot = bytes.iter().position(|&byte| byte == b'.');
    let (base, extension) = match dot {
        Some(index) => (&bytes[..index], &bytes[index + 1..]),
        None => (bytes, &[][..]),
    };
    if base.is_empty() || base.len() > 8 || extension.len() > 3 {
        return Err("name is not 8.3 format");
    }
    let mut output = [b' '; 11];
    for (index, byte) in base.iter().enumerate() {
        output[index] = byte.to_ascii_uppercase();
    }
    for (index, byte) in extension.iter().enumerate() {
        output[8 + index] = byte.to_ascii_uppercase();
    }
    Ok(output)
}
