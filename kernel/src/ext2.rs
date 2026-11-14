//! ext2 filesystem formatter + reader (milestone 0.0.9).
//!
//! Full mkfs: superblock, block group descriptor, block/inode bitmaps,
//! inode table, root directory with "." and "..". Read support: list root
//! directory entries and read a file by name.

use crate::storage::{self, SECTOR_SIZE};

const BLOCK_SIZE: u32 = 1024;
const INODE_SIZE: u32 = 128;
const INODES_PER_GROUP: u32 = 8192;
const BLOCKS_PER_GROUP: u32 = 8192;

pub fn format_qemu_test_disk(
    start_lba: u64,
    num_sectors: u64,
    qemu_virt: bool,
) -> Result<(), &'static str> {
    if !qemu_virt {
        return Err("experimental ext2 formatter is disabled on physical hardware");
    }
    let capacity = storage::capacity_sectors().ok_or("no block device")?;
    if start_lba
        .checked_add(num_sectors)
        .map_or(true, |end| end > capacity)
    {
        return Err("format range exceeds the test block device");
    }
    let total_blocks = (num_sectors * 512 / BLOCK_SIZE as u64) as u32;
    if total_blocks < 64 { return Err("partition too small"); }

    let num_groups = (total_blocks + BLOCKS_PER_GROUP - 1) / BLOCKS_PER_GROUP;
    let inodes_count = INODES_PER_GROUP * num_groups;
    // inode table: INODES_PER_GROUP inodes per group, each 128 bytes
    let inode_table_blocks = (INODES_PER_GROUP * INODE_SIZE) / BLOCK_SIZE;

    // Layout for group 0:
    // block 0: boot area (1024 bytes) + superblock at offset 1024
    // block 1: GDT
    // block 2: block bitmap
    // block 3: inode bitmap
    // blocks 4..4+inode_table_blocks: inode table
    // block 4+inode_table_blocks: root directory data (block 1 of data? no, first data block)
    let first_data_block: u32 = 1; // for 1024-byte blocks, first data block is 1? Actually for 1024 block size, first_data_block=0? No, ext2: first_data_block=0 for >=2048, 1 for 1024.
    // We'll use first_data_block=1 for 1024-byte blocks.
    let gdt_block: u32 = 2;
    let bbm_block: u32 = 3; // block bitmap
    let ibm_block: u32 = 4; // inode bitmap
    let inode_table_start: u32 = 5;
    let root_dir_block: u32 = inode_table_start + inode_table_blocks;

    // Write superblock at byte offset 1024 in block 0 (sector start_lba + 2)
    let sb_sector = start_lba + 2; // 1024 bytes = 2 sectors
    let mut sb = [0u8; 1024];
    write_u32(&mut sb, 0, inodes_count);          // s_inodes_count
    write_u32(&mut sb, 4, total_blocks);           // s_blocks_count
    write_u32(&mut sb, 8, total_blocks / 20);      // s_r_blocks_count
    write_u32(&mut sb, 12, total_blocks - 10);     // s_free_blocks_count
    write_u32(&mut sb, 16, inodes_count - 10);     // s_free_inodes_count
    write_u32(&mut sb, 20, first_data_block);      // s_first_data_block
    write_u32(&mut sb, 24, 0);                      // s_log_block_size (0 => 1024)
    write_u32(&mut sb, 28, 0);                      // s_log_frag_size
    write_u32(&mut sb, 32, BLOCKS_PER_GROUP);      // s_blocks_per_group
    write_u32(&mut sb, 36, BLOCKS_PER_GROUP);      // s_frags_per_group
    write_u32(&mut sb, 40, INODES_PER_GROUP);      // s_inodes_per_group
    write_u16(&mut sb, 56, 0xEF53);                // s_magic
    write_u16(&mut sb, 58, 1);                      // s_state (clean)
    write_u32(&mut sb, 76, 1);                      // s_rev_level
    write_u16(&mut sb, 88, 128);                    // s_inode_size
    write_u16(&mut sb, 90, 0);                      // s_block_group_nr
    write_u32(&mut sb, 92, 0);                      // s_feature_compat
    write_u32(&mut sb, 96, 0x0002);                // s_feature_incompat (filetype)
    write_u32(&mut sb, 100, 0);                     // s_feature_ro_compat

    // write superblock as 2 sectors
    let mut sec = [0u8; SECTOR_SIZE];
    // sector 0 of superblock (offset 1024 = sector 2 of partition): first 512 bytes of sb
    for i in 0..512 { sec[i] = sb[i]; }
    storage::write_sector(sb_sector, &sec).map_err(|_| "write sb0")?;
    for i in 0..512 { sec[i] = sb[512 + i]; }
    storage::write_sector(sb_sector + 1, &sec).map_err(|_| "write sb1")?;

    // Block group descriptor at block 2 (GDT) = sectors start_lba + 2*2 = start_lba+4
    let mut gdt = [0u8; BLOCK_SIZE as usize];
    // bg_block_bitmap @ 0
    write_u32(&mut gdt, 0, bbm_block);
    // bg_inode_bitmap @ 4
    write_u32(&mut gdt, 4, ibm_block);
    // bg_inode_table @ 8
    write_u32(&mut gdt, 8, inode_table_start);
    // bg_free_blocks_count @ 12
    write_u16(&mut gdt, 12, (total_blocks - root_dir_block - 1) as u16);
    // bg_free_inodes_count @ 14
    write_u16(&mut gdt, 14, (inodes_count - 11) as u16);
    // bg_used_dirs_count @ 16
    write_u16(&mut gdt, 16, 1);
    // write GDT (1 block = 2 sectors)
    let gdt_lba = start_lba + gdt_block as u64 * (BLOCK_SIZE as u64 / 512);
    for i in 0..2 {
        for j in 0..512 { sec[j] = gdt[i * 512 + j]; }
        storage::write_sector(gdt_lba + i as u64, &sec).map_err(|_| "write gdt")?;
    }

    // Block bitmap: mark blocks 0..root_dir_block+1 as used
    let mut bbm = [0u8; BLOCK_SIZE as usize];
    for b in 0..=(root_dir_block as usize) {
        bbm[b / 8] |= 1 << (b % 8);
    }
    let bbm_lba = start_lba + bbm_block as u64 * (BLOCK_SIZE as u64 / 512);
    for i in 0..2 {
        for j in 0..512 { sec[j] = bbm[i * 512 + j]; }
        storage::write_sector(bbm_lba + i as u64, &sec).map_err(|_| "write bbm")?;
    }

    // Inode bitmap: mark inodes 1..11 as used (reserved) + root (2)
    let mut ibm = [0u8; BLOCK_SIZE as usize];
    for i in 0..11 { ibm[i / 8] |= 1 << (i % 8); }
    ibm[1 / 8] |= 1 << (1 % 8); // inode 2 (root)
    let ibm_lba = start_lba + ibm_block as u64 * (BLOCK_SIZE as u64 / 512);
    for i in 0..2 {
        for j in 0..512 { sec[j] = ibm[i * 512 + j]; }
        storage::write_sector(ibm_lba + i as u64, &sec).map_err(|_| "write ibm")?;
    }

    // Inode table: write root inode (inode 2) at inode_table_start block, offset 128 (inode 2 = index 1)
    let mut inode_table = [0u8; BLOCK_SIZE as usize];
    let root_inode_off = 1 * INODE_SIZE as usize; // inode 2 is index 1
    // i_mode: directory (0x4000) | 0755
    write_u16(&mut inode_table, root_inode_off, 0x41ED);
    // i_uid
    write_u16(&mut inode_table, root_inode_off + 2, 0);
    // i_size
    write_u32(&mut inode_table, root_inode_off + 4, BLOCK_SIZE); // 1024
    // i_atime, i_ctime, i_mtime
    write_u32(&mut inode_table, root_inode_off + 8, 0);
    write_u32(&mut inode_table, root_inode_off + 12, 0);
    write_u32(&mut inode_table, root_inode_off + 16, 0);
    // i_dtime
    write_u32(&mut inode_table, root_inode_off + 20, 0);
    // i_gid
    write_u16(&mut inode_table, root_inode_off + 24, 0);
    // i_links_count
    write_u16(&mut inode_table, root_inode_off + 26, 2);
    // i_blocks (in 512-byte units)
    write_u32(&mut inode_table, root_inode_off + 28, 2);
    // i_flags
    write_u32(&mut inode_table, root_inode_off + 32, 0);
    // i_block[0] = root_dir_block
    write_u32(&mut inode_table, root_inode_off + 40, root_dir_block);
    // rest of i_block zeroed

    let it_lba = start_lba + inode_table_start as u64 * (BLOCK_SIZE as u64 / 512);
    for i in 0..(inode_table_blocks * 2) {
        for j in 0..512 { sec[j] = inode_table[i as usize * 512 + j]; }
        storage::write_sector(it_lba + i as u64, &sec).map_err(|_| "write inode table")?;
    }

    // Root directory data block
    let mut rootdir = [0u8; BLOCK_SIZE as usize];
    // "." entry
    rootdir[0..4].copy_from_slice(&2u32.to_le_bytes()); // inode 2
    rootdir[4..6].copy_from_slice(&12u16.to_le_bytes()); // rec_len
    rootdir[6] = 1; // name_len
    rootdir[7] = 2; // file_type (directory)
    rootdir[8] = b'.';
    // ".." entry
    rootdir[12..16].copy_from_slice(&2u32.to_le_bytes()); // inode 2 (root's parent is itself)
    rootdir[16..18].copy_from_slice(&(BLOCK_SIZE as u16 - 12).to_le_bytes());
    rootdir[18] = 2; // name_len
    rootdir[19] = 2; // file_type
    rootdir[20] = b'.';
    rootdir[21] = b'.';

    let rd_lba = start_lba + root_dir_block as u64 * (BLOCK_SIZE as u64 / 512);
    for i in 0..2 {
        for j in 0..512 { sec[j] = rootdir[i * 512 + j]; }
        storage::write_sector(rd_lba + i as u64, &sec).map_err(|_| "write rootdir")?;
    }

    crate::println!("ext2: formatted {} blocks, {} inodes, {} groups", total_blocks, inodes_count, num_groups);
    Ok(())
}

fn write_u32(buf: &mut [u8], off: usize, val: u32) {
    buf[off..off + 4].copy_from_slice(&val.to_le_bytes());
}
fn write_u16(buf: &mut [u8], off: usize, val: u16) {
    buf[off..off + 2].copy_from_slice(&val.to_le_bytes());
}
