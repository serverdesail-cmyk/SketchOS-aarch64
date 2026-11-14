//! Storage subsystem (milestone 0.0.7).

use core::sync::atomic::Ordering;

use crate::println;
use crate::sync::IrqSpinLock;
use sketchos_bootinfo::{phys_to_virt, PHYSMAP_BASE};

pub const SECTOR_SIZE: usize = 512;

pub trait BlockDevice {
    fn capacity_sectors(&self) -> u64;
    fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), &'static str>;
    fn write(&self, lba: u64, buf: &[u8]) -> Result<(), &'static str>;
    fn name(&self) -> &'static str;
}

static DEVICE: IrqSpinLock<Option<VirtioMmioBlock>> = IrqSpinLock::new(None);

pub fn register(dev: VirtioMmioBlock) {
    *DEVICE.lock() = Some(dev);
}

pub fn capacity_sectors() -> Option<u64> {
    DEVICE.lock().as_ref().map(|device| device.capacity_sectors())
}

pub fn device_name() -> Option<&'static str> {
    DEVICE.lock().as_ref().map(|device| device.name())
}

pub fn report() {
    if let Some(name) = device_name() {
        if let Some(capacity) = capacity_sectors() {
            println!("Storage: {} ({} sectors)", name, capacity);
        }
    } else {
        println!("Storage: no block device");
    }
}

pub mod virtio {
    use super::*;
    use crate::phys;

    const MAGIC: u32 = 0x74726976;

    const REG_MAGIC: usize = 0x00;
    const REG_VERSION: usize = 0x04;
    const REG_DEVICE_ID: usize = 0x08;
    const REG_GUEST_PAGE_SIZE: usize = 0x28;
    const REG_QUEUE_SEL: usize = 0x30;
    const REG_QUEUE_NUM_MAX: usize = 0x34;
    const REG_QUEUE_NUM: usize = 0x38;
    const REG_QUEUE_ALIGN: usize = 0x3c;
    const REG_QUEUE_PFN: usize = 0x40;
    const REG_QUEUE_NOTIFY: usize = 0x50;
    const REG_STATUS: usize = 0x70;
    const REG_CONFIG: usize = 0x100;

    const STATUS_ACK: u32 = 1;
    const STATUS_DRIVER: u32 = 2;
    const STATUS_DRIVER_OK: u32 = 4;

    #[repr(C)]
    pub struct VirtqDesc {
        pub addr: u64,
        pub len: u32,
        pub flags: u16,
        pub next: u16,
    }

    pub struct VirtioMmioBlock {
        base: u64,
        pub capacity: u64,
        queue_phys: u64,
        queue_size: u16,
        desc: *mut VirtqDesc,
        avail_idx: *mut u16,
        used_idx: *mut u16,
    }

    // SAFETY: the block device is moved into DEVICE and all access is serialized
    // by the enclosing IrqSpinLock; the raw queue pointers never escape it.
    unsafe impl Send for VirtioMmioBlock {}

    impl VirtioMmioBlock {
        pub fn probe(base: u64) -> Option<Self> {
            let magic = mmio_r(base, REG_MAGIC);
            if magic != MAGIC { return None; }
            if mmio_r(base, REG_VERSION) != 1 { return None; }
            if mmio_r(base, REG_DEVICE_ID) != 2 { return None; }

            mmio_w(base, REG_STATUS, 0);
            mmio_w(base, REG_STATUS, STATUS_ACK | STATUS_DRIVER);
            mmio_w(base, REG_GUEST_PAGE_SIZE, 4096);

            mmio_w(base, REG_QUEUE_SEL, 0);
            let max = mmio_r(base, REG_QUEUE_NUM_MAX);
            if max == 0 { return None; }
            let qsz: u16 = max.min(8) as u16;
            mmio_w(base, REG_QUEUE_NUM, qsz as u32);
            mmio_w(base, REG_QUEUE_ALIGN, 4096);

            let queue_phys = phys::alloc_pages(2, 4096)?;
            zero_page(queue_phys);
            zero_page(queue_phys + 4096);

            let avail_ring_phys = queue_phys + (16 * qsz as u64);
            let used_ring_phys = queue_phys + 4096;

            mmio_w(base, REG_QUEUE_PFN, (queue_phys >> 12) as u32);

            mmio_w(base, REG_STATUS, STATUS_ACK | STATUS_DRIVER | STATUS_DRIVER_OK);

            let cap_lo = mmio_r(base, REG_CONFIG) as u64;
            let cap_hi = mmio_r(base, REG_CONFIG + 4) as u64;
            let capacity = cap_lo | (cap_hi << 32);

            let desc = phys_to_virt(queue_phys) as *mut VirtqDesc;
            let avail_idx = unsafe { (phys_to_virt(avail_ring_phys) as *mut u8).add(2) as *mut u16 };
            let used_idx = unsafe { (phys_to_virt(used_ring_phys) as *mut u8).add(2) as *mut u16 };

            Some(Self {
                base,
                capacity,
                queue_phys,
                queue_size: qsz,
                desc,
                avail_idx,
                used_idx,
            })
        }

        pub fn read_sector(&self, lba: u64, buf_phys: u64) -> Result<(), &'static str> {
            unsafe {
                let hdr_phys = phys::alloc_pages(1, 4096).ok_or("oom")?;
                zero_page(hdr_phys);
                let hdr = phys_to_virt(hdr_phys) as *mut u32;
                hdr.write_volatile(0); // type = IN
                hdr.add(1).write_volatile(0);
                (hdr.add(2) as *mut u64).write_volatile(lba);

                let status_phys = phys::alloc_pages(1, 4096).ok_or("oom")?;
                zero_page(status_phys);
                let status_ptr = phys_to_virt(status_phys) as *mut u8;

                let d0 = &mut *self.desc.add(0);
                d0.addr = hdr_phys;
                d0.len = 16;
                d0.flags = 1; // NEXT
                d0.next = 1;
                let d1 = &mut *self.desc.add(1);
                d1.addr = buf_phys;
                d1.len = SECTOR_SIZE as u32;
                d1.flags = 2 | 1; // WRITE | NEXT
                d1.next = 2;
                let d2 = &mut *self.desc.add(2);
                d2.addr = status_phys;
                d2.len = 1;
                d2.flags = 2; // WRITE
                d2.next = 0;

                let used_before = self.used_idx.read_volatile();
                let cur_idx = self.avail_idx.read_volatile();
                let ring_slot = (self.queue_phys + 16 * self.queue_size as u64 + 4) as *mut u16;
                ring_slot.add((cur_idx as usize) % (self.queue_size as usize)).write_volatile(0);
                core::sync::atomic::fence(Ordering::SeqCst);
                self.avail_idx.write_volatile(cur_idx.wrapping_add(1));

                mmio_w(self.base, REG_QUEUE_NOTIFY, 0);

                let mut spins = 0u32;
                loop {
                    core::sync::atomic::fence(Ordering::SeqCst);
                    if self.used_idx.read_volatile() != used_before {
                        break;
                    }
                    spins += 1;
                    if spins > 10_000_000 {
                        return Err("virtio timeout");
                    }
                }

                let st = status_ptr.read_volatile();
                if st != 0 {
                    return Err("virtio I/O error");
                }
                Ok(())
            }
        }

        pub fn write_sector(&self, lba: u64, buf: &[u8]) -> Result<(), &'static str> {
            if buf.len() < SECTOR_SIZE {
                return Err("buffer too small");
            }
            unsafe {
                let hdr_phys = phys::alloc_pages(1, 4096).ok_or("oom")?;
                zero_page(hdr_phys);
                let hdr = phys_to_virt(hdr_phys) as *mut u32;
                hdr.write_volatile(1); // type = OUT (write)
                hdr.add(1).write_volatile(0);
                (hdr.add(2) as *mut u64).write_volatile(lba);

                let data_phys = virt_to_phys(buf.as_ptr() as u64).ok_or("bad buffer")?;

                let status_phys = phys::alloc_pages(1, 4096).ok_or("oom")?;
                zero_page(status_phys);
                let status_ptr = phys_to_virt(status_phys) as *mut u8;

                let d0 = &mut *self.desc.add(0);
                d0.addr = hdr_phys;
                d0.len = 16;
                d0.flags = 1; // NEXT
                d0.next = 1;
                let d1 = &mut *self.desc.add(1);
                d1.addr = data_phys;
                d1.len = SECTOR_SIZE as u32;
                d1.flags = 1; // NEXT (device reads)
                d1.next = 2;
                let d2 = &mut *self.desc.add(2);
                d2.addr = status_phys;
                d2.len = 1;
                d2.flags = 2; // WRITE
                d2.next = 0;

                let used_before = self.used_idx.read_volatile();
                let cur_idx = self.avail_idx.read_volatile();
                let ring_slot = (self.queue_phys + 16 * self.queue_size as u64 + 4) as *mut u16;
                ring_slot.add((cur_idx as usize) % (self.queue_size as usize)).write_volatile(0);
                core::sync::atomic::fence(Ordering::SeqCst);
                self.avail_idx.write_volatile(cur_idx.wrapping_add(1));

                mmio_w(self.base, REG_QUEUE_NOTIFY, 0);

                let mut spins = 0u32;
                loop {
                    core::sync::atomic::fence(Ordering::SeqCst);
                    if self.used_idx.read_volatile() != used_before {
                        break;
                    }
                    spins += 1;
                    if spins > 10_000_000 {
                        return Err("virtio timeout");
                    }
                }

                let st = status_ptr.read_volatile();
                if st != 0 {
                    return Err("virtio I/O error");
                }
                Ok(())
            }
        }
    }

    impl BlockDevice for VirtioMmioBlock {
        fn capacity_sectors(&self) -> u64 {
            self.capacity
        }
        fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), &'static str> {
            if buf.len() < SECTOR_SIZE {
                return Err("buffer too small");
            }
            let buf_phys = virt_to_phys(buf.as_ptr() as u64).ok_or("bad buffer")?;
            self.read_sector(lba, buf_phys)
        }
        fn write(&self, lba: u64, buf: &[u8]) -> Result<(), &'static str> {
            self.write_sector(lba, buf)
        }
        fn name(&self) -> &'static str {
            "virtio-blk (mmio, legacy)"
        }
    }

    fn mmio_r(base: u64, off: usize) -> u32 {
        let p = (phys_to_virt(base) as usize + off) as *const u32;
        unsafe { core::ptr::read_volatile(p) }
    }
    fn mmio_w(base: u64, off: usize, val: u32) {
        let p = (phys_to_virt(base) as usize + off) as *mut u32;
        unsafe { core::ptr::write_volatile(p, val) }
    }
    fn zero_page(phys: u64) {
        let v = phys_to_virt(phys) as *mut u8;
        unsafe { core::ptr::write_bytes(v, 0, 4096) }
    }
    fn virt_to_phys(v: u64) -> Option<u64> {
        if v < PHYSMAP_BASE { return None; }
        let p = v - PHYSMAP_BASE;
        if p < (512u64 * 1024 * 1024 * 1024) { Some(p) } else { None }
    }
}

pub use virtio::VirtioMmioBlock;

/// Probe the QEMU virt machine's fixed VirtIO-MMIO slot range.
///
/// This address range is not a generic AArch64 device bus. Never call this on
/// physical hardware; platform devices must be discovered from ACPI/PCI first.
pub fn probe_qemu_virtio_mmio() {
    let mut base = 0x0a00_0000u64;
    for _ in 0..32 {
        if let Some(dev) = VirtioMmioBlock::probe(base) {
            println!("Storage: found {} at 0x{:016x}, {} sectors", dev.name(), base, dev.capacity_sectors());
            register(dev);
            return;
        }
        base += 0x200;
    }
    println!("Storage: no virtio-blk (mmio) device found");
}

pub fn read_sector(lba: u64, buf: &mut [u8]) -> Result<(), &'static str> {
    let guard = DEVICE.lock();
    let dev = guard.as_ref().ok_or("no block device")?;
    dev.read(lba, buf)
}

pub fn write_sector(lba: u64, buf: &[u8]) -> Result<(), &'static str> {
    let guard = DEVICE.lock();
    let dev = guard.as_ref().ok_or("no block device")?;
    dev.write(lba, buf)
}
