//! FAT32 read/write filesystem (milestone 0.0.7).

use crate::phys;
use crate::storage::{self, BlockDevice, SECTOR_SIZE};

pub struct Fat32 {
    part_lba: u64,
    sectors_per_cluster: u32,
    num_fats: u32,
    fat_size_sectors: u32,
    root_cluster: u32,
    data_start_lba: u64,
    fat_start_lba: u64,
    _sector_buf: *mut u8,
}

static mut FS: Option<Fat32> = None;

pub fn fs() -> Option<&'static mut Fat32> {
    unsafe { FS.as_mut() }
}

impl Fat32 {
    pub fn mount() -> Result<(), &'static str> {
        let dev = storage::device().ok_or("no block device")?;
        let _buf_phys = phys::alloc_pages(1, 4096).ok_or("oom")?;

        let mut sector0 = [0u8; SECTOR_SIZE];
        dev.read(0, &mut sector0).map_err(|_| "read sector 0")?;

        let part_lba: u64 = if sector0[510] == 0x55 && sector0[511] == 0xAA {
            let lba = u32::from_le_bytes([sector0[0x1BE + 8], sector0[0x1BE + 9], sector0[0x1BE + 10], sector0[0x1BE + 11]]);
            if lba == 0 { 0 } else { lba as u64 }
        } else {
            0
        };

        let mut bpb = [0u8; SECTOR_SIZE];
        dev.read(part_lba, &mut bpb).map_err(|_| "read bpb")?;

        let bytes_per_sector = u16::from_le_bytes([bpb[0x0B], bpb[0x0C]]) as u32;
        if bytes_per_sector != 512 {
            return Err("unsupported sector size");
        }
        let sectors_per_cluster = bpb[0x0D] as u32;
        let reserved_sectors = u16::from_le_bytes([bpb[0x0E], bpb[0x0F]]) as u32;
        let num_fats = bpb[0x10] as u32;
        let fat_size_16 = u16::from_le_bytes([bpb[0x16], bpb[0x17]]) as u32;
        let fat_size_32 = u32::from_le_bytes([bpb[0x24], bpb[0x25], bpb[0x26], bpb[0x27]]);
        let root_cluster = u32::from_le_bytes([bpb[0x2C], bpb[0x2D], bpb[0x2E], bpb[0x2F]]);

        let fat_size_sectors = if fat_size_32 != 0 { fat_size_32 } else { fat_size_16 };
        if fat_size_sectors == 0 {
            return Err("not fat32");
        }

        let fat_start_lba = part_lba + reserved_sectors as u64;
        let data_start_lba = fat_start_lba + (num_fats * fat_size_sectors) as u64;

        let fs = Fat32 {
            part_lba,
            sectors_per_cluster,
            num_fats,
            fat_size_sectors,
            root_cluster,
            data_start_lba,
            fat_start_lba,
            _sector_buf: core::ptr::null_mut(),
        };
        unsafe { FS = Some(fs); }
        Ok(())
    }

    fn cluster_to_lba(&self, cluster: u32) -> u64 {
        self.data_start_lba + (cluster as u64 - 2) * self.sectors_per_cluster as u64
    }

    fn read_fat_entry(&self, cluster: u32) -> u32 {
        let dev = storage::device().unwrap();
        let fat_offset = cluster * 4;
        let fat_sector = self.fat_start_lba + (fat_offset / 512) as u64;
        let off = (fat_offset % 512) as usize;
        let mut sec = [0u8; SECTOR_SIZE];
        let _ = dev.read(fat_sector, &mut sec);
        let val = u32::from_le_bytes([sec[off], sec[off + 1], sec[off + 2], sec[off + 3]]);
        val & 0x0FFF_FFFF
    }

    pub fn list_root(&self) {
        let dev = storage::device().unwrap();
        let spc = self.sectors_per_cluster;
        let mut buf = [0u8; SECTOR_SIZE];
        let mut cluster = self.root_cluster;
        while cluster < 0x0FFF_FFF8 {
            for s in 0..spc {
                let lba = self.cluster_to_lba(cluster) + s as u64;
                if dev.read(lba, &mut buf).is_err() { return; }
                for i in 0..16 {
                    let off = i * 32;
                    let first = buf[off];
                    if first == 0 { return; }
                    if first == 0xE5 { continue; }
                    let attr = buf[off + 0x0B];
                    if attr == 0x0F { continue; }
                    if attr & 0x08 != 0 { continue; }
                    let mut name = [0u8; 13];
                    let mut ni = 0usize;
                    for j in 0..8 {
                        let c = buf[off + j];
                        if c == b' ' { break; }
                        name[ni] = c; ni += 1;
                    }
                    let mut has_ext = false;
                    for j in 0..3 { if buf[off + 8 + j] != b' ' { has_ext = true; break; } }
                    if has_ext {
                        name[ni] = b'.'; ni += 1;
                        for j in 0..3 {
                            let c = buf[off + 8 + j];
                            if c == b' ' { break; }
                            name[ni] = c; ni += 1;
                        }
                    }
                    let name_str = core::str::from_utf8(&name[..ni]).unwrap_or("?");
                    let size = u32::from_le_bytes([buf[off + 0x1C], buf[off + 0x1D], buf[off + 0x1E], buf[off + 0x1F]]);
                    let is_dir = attr & 0x10 != 0;
                    crate::println!("  {}  {} bytes{}", name_str, size, if is_dir { "  <dir>" } else { "" });
                }
            }
            cluster = self.read_fat_entry(cluster);
        }
    }

    pub fn read_file(&self, name: &str, buf: &mut [u8]) -> Result<usize, &'static str> {
        let dev = storage::device().unwrap();
        let mut sec = [0u8; SECTOR_SIZE];
        let mut found_cluster: u32 = 0;
        let mut found_size: u32 = 0;
        let mut found = false;
        let mut cluster = self.root_cluster;
        let spc = self.sectors_per_cluster;
        while cluster < 0x0FFF_FFF8 && !found {
            for s in 0..spc {
                let lba = self.cluster_to_lba(cluster) + s as u64;
                if dev.read(lba, &mut sec).is_err() { return Err("io"); }
                for i in 0..16 {
                    let off = i * 32;
                    if sec[off] == 0 { return Err("not found"); }
                    if sec[off] == 0xE5 { continue; }
                    if sec[off + 0x0B] == 0x0F { continue; }
                    let mut nm = [0u8; 13];
                    let mut ni = 0;
                    for j in 0..8 { let c = sec[off + j]; if c == b' ' { break; } nm[ni] = c; ni += 1; }
                    let mut has_ext = false;
                    for j in 0..3 { if sec[off + 8 + j] != b' ' { has_ext = true; break; } }
                    if has_ext { nm[ni] = b'.'; ni += 1; for j in 0..3 { let c = sec[off + 8 + j]; if c == b' ' { break; } nm[ni] = c; ni += 1; } }
                    if let Ok(n) = core::str::from_utf8(&nm[..ni]) {
                        if n.eq_ignore_ascii_case(name) {
                            found_size = u32::from_le_bytes([sec[off + 0x1C], sec[off + 0x1D], sec[off + 0x1E], sec[off + 0x1F]]);
                            let hi = u16::from_le_bytes([sec[off + 0x14], sec[off + 0x15]]) as u32;
                            let lo = u16::from_le_bytes([sec[off + 0x1A], sec[off + 0x1B]]) as u32;
                            found_cluster = (hi << 16) | lo;
                            found = true;
                            break;
                        }
                    }
                }
                if found { break; }
            }
            if !found { cluster = self.read_fat_entry(cluster); }
        }
        if !found { return Err("not found"); }

        let mut remaining = found_size as usize;
        let mut written = 0usize;
        let mut cl = found_cluster;
        let spc = self.sectors_per_cluster;
        while remaining > 0 && cl < 0x0FFF_FFF8 {
            for s in 0..spc {
                if remaining == 0 { break; }
                let lba = self.cluster_to_lba(cl) + s as u64;
                if dev.read(lba, &mut sec).is_err() { return Err("io"); }
                let take = remaining.min(SECTOR_SIZE);
                if written + take <= buf.len() {
                    buf[written..written + take].copy_from_slice(&sec[..take]);
                }
                written += take;
                remaining = remaining.saturating_sub(take);
            }
            cl = self.read_fat_entry(cl);
        }
        Ok(written.min(buf.len()))
    }

    pub fn write_file(&self, name: &str, data: &[u8]) -> Result<(), &'static str> {
        let bytes_per_cluster = self.sectors_per_cluster as usize * SECTOR_SIZE;
        let clusters_needed = (data.len() + bytes_per_cluster - 1) / bytes_per_cluster.max(1);
        let clusters_needed = clusters_needed.max(1);

        let mut allocated = [0u32; 64];
        let mut nalloc = 0usize;
        let mut c = 2u32;
        while nalloc < clusters_needed {
            if self.read_fat_entry(c) == 0 {
                allocated[nalloc] = c;
                nalloc += 1;
            }
            c += 1;
            if c > 0x0FFF_FFEF { return Err("no free clusters"); }
        }

        for i in 0..nalloc {
            let next = if i + 1 < nalloc { allocated[i + 1] } else { 0x0FFF_FFFF };
            self.write_fat_entry(allocated[i], next);
        }

        let dev = storage::device().unwrap();
        let mut off = 0usize;
        for i in 0..nalloc {
            for s in 0..self.sectors_per_cluster {
                if off >= data.len() { break; }
                let mut sec = [0u8; SECTOR_SIZE];
                let take = (data.len() - off).min(SECTOR_SIZE);
                sec[..take].copy_from_slice(&data[off..off + take]);
                let lba = self.cluster_to_lba(allocated[i]) + s as u64;
                dev.write(lba, &sec).map_err(|_| "write data")?;
                off += take;
            }
        }

        let mut sec = [0u8; SECTOR_SIZE];
        let mut cluster = self.root_cluster;
        let spc = self.sectors_per_cluster;
        'outer: while cluster < 0x0FFF_FFF8 {
            for s in 0..spc {
                let lba = self.cluster_to_lba(cluster) + s as u64;
                if dev.read(lba, &mut sec).is_err() { return Err("io"); }
                for i in 0..16 {
                    let o = i * 32;
                    if sec[o] == 0 || sec[o] == 0xE5 {
                        let mut entry = [0u8; 32];
                        let name_bytes = name.as_bytes();
                        let (base, ext) = split_name(name_bytes);
                        for j in 0..8 { entry[j] = if j < base.len() { base[j] } else { b' ' }; }
                        for j in 0..3 { entry[8 + j] = if j < ext.len() { ext[j] } else { b' ' }; }
                        entry[0x0B] = 0x20;
                        let hi = (allocated[0] >> 16) as u16;
                        let lo = (allocated[0] & 0xFFFF) as u16;
                        entry[0x14..0x16].copy_from_slice(&hi.to_le_bytes());
                        entry[0x1A..0x1C].copy_from_slice(&lo.to_le_bytes());
                        let size = data.len() as u32;
                        entry[0x1C..0x20].copy_from_slice(&size.to_le_bytes());
                        sec[o..o + 32].copy_from_slice(&entry);
                        dev.write(lba, &sec).map_err(|_| "write dir")?;
                        break 'outer;
                    }
                }
            }
            cluster = self.read_fat_entry(cluster);
        }
        Ok(())
    }

    fn write_fat_entry(&self, cluster: u32, value: u32) {
        let dev = storage::device().unwrap();
        let fat_offset = cluster * 4;
        let fat_sector = self.fat_start_lba + (fat_offset / 512) as u64;
        let off = (fat_offset % 512) as usize;
        let mut sec = [0u8; SECTOR_SIZE];
        let _ = dev.read(fat_sector, &mut sec);
        sec[off..off + 4].copy_from_slice(&value.to_le_bytes());
        let _ = dev.write(fat_sector, &sec);
    }
}

fn split_name(name: &[u8]) -> (&[u8], &[u8]) {
    let mut dot = None;
    for (i, &c) in name.iter().enumerate() {
        if c == b'.' { dot = Some(i); }
    }
    match dot {
        Some(i) => (&name[..i], &name[i + 1..]),
        None => (name, &name[name.len()..]),
    }
}
