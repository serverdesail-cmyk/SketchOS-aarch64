//! Network stack (milestone 0.0.9, scaffold).

pub const ETH_ALEN: usize = 6;

#[derive(Clone, Copy)]
pub struct Mac(pub [u8; ETH_ALEN]);

#[derive(Clone, Copy, PartialEq)]
pub struct Ipv4(pub [u8; 4]);

impl Ipv4 {
    pub const fn new(a: u8, b: u8, c: u8, d: u8) -> Self { Ipv4([a, b, c, d]) }
}

pub static mut MY_IP: Ipv4 = Ipv4([10, 0, 2, 15]);
pub static mut MY_MAC: Mac = Mac([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);

pub mod virtio_net {
    use super::*;

    const MAGIC: u32 = 0x74726976;

    pub struct VirtioNet {
        pub mac: Mac,
        base: u64,
    }

    unsafe impl Sync for VirtioNet {}
    unsafe impl Send for VirtioNet {}

    impl VirtioNet {
        pub fn probe(base: u64) -> Option<Self> {
            let magic = unsafe { core::ptr::read_volatile(sketchos_bootinfo::phys_to_virt(base) as *const u32) };
            if magic != MAGIC { return None; }
            let ver = unsafe { core::ptr::read_volatile((sketchos_bootinfo::phys_to_virt(base) + 4) as *const u32) };
            if ver != 1 { return None; }
            let dev_id = unsafe { core::ptr::read_volatile((sketchos_bootinfo::phys_to_virt(base) + 8) as *const u32) };
            if dev_id != 1 { return None; }
            let mut mac = [0u8; ETH_ALEN];
            for i in 0..ETH_ALEN {
                mac[i] = unsafe { core::ptr::read_volatile((sketchos_bootinfo::phys_to_virt(base) + 0x100 + i as u64) as *const u8) };
            }
            crate::println!("Net: found virtio-net at 0x{:08x}, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", base, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
            unsafe { MY_MAC = Mac(mac); }
            Some(Self { mac: Mac(mac), base })
        }
    }
}

static mut NETDEV: Option<virtio_net::VirtioNet> = None;

pub fn probe() {
    let mut base = 0x0a00_0000u64;
    for _ in 0..32 {
        if let Some(dev) = virtio_net::VirtioNet::probe(base) {
            unsafe { NETDEV = Some(dev); }
            return;
        }
        base += 0x200;
    }
    crate::println!("Net: no virtio-net (mmio) device found");
}

pub fn report() {
    unsafe {
        if let Some(dev) = NETDEV.as_ref() {
            crate::println!("Net: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} IP {}.{}.{}.{}", dev.mac.0[0], dev.mac.0[1], dev.mac.0[2], dev.mac.0[3], dev.mac.0[4], dev.mac.0[5], MY_IP.0[0], MY_IP.0[1], MY_IP.0[2], MY_IP.0[3]);
        } else {
            crate::println!("Net: no network device");
        }
    }
}

pub fn device() -> Option<&'static mut virtio_net::VirtioNet> {
    unsafe { NETDEV.as_mut() }
}
