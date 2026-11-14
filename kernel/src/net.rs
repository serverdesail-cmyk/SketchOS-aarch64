//! Network stack scaffold. VirtIO-MMIO probing is reserved for QEMU virt.

use crate::sync::IrqSpinLock;

pub const ETH_ALEN: usize = 6;

#[derive(Clone, Copy)]
pub struct Mac(pub [u8; ETH_ALEN]);

#[derive(Clone, Copy, PartialEq)]
pub struct Ipv4(pub [u8; 4]);

impl Ipv4 {
    pub const fn new(a: u8, b: u8, c: u8, d: u8) -> Self {
        Self([a, b, c, d])
    }
}

pub mod virtio_net {
    use super::*;

    const MAGIC: u32 = 0x7472_6976;

    #[derive(Clone, Copy)]
    pub struct VirtioNet {
        pub mac: Mac,
        base: u64,
    }

    impl VirtioNet {
        /// QEMU-only VirtIO-MMIO identification; caller must gate by platform.
        pub fn probe_qemu_slot(base: u64) -> Option<Self> {
            let virtual_base = sketchos_bootinfo::phys_to_virt(base);
            // SAFETY: this helper is only used for the QEMU virt machine's
            // explicitly mapped VirtIO-MMIO aperture.
            let magic = unsafe { core::ptr::read_volatile(virtual_base as *const u32) };
            if magic != MAGIC {
                return None;
            }
            let version = unsafe {
                core::ptr::read_volatile((virtual_base + 4) as *const u32)
            };
            if version != 1 {
                return None;
            }
            let device_id = unsafe {
                core::ptr::read_volatile((virtual_base + 8) as *const u32)
            };
            if device_id != 1 {
                return None;
            }
            let mut mac = [0u8; ETH_ALEN];
            for (index, byte) in mac.iter_mut().enumerate() {
                *byte = unsafe {
                    core::ptr::read_volatile((virtual_base + 0x100 + index as u64) as *const u8)
                };
            }
            crate::println!(
                "Net: found QEMU virtio-net at 0x{:08x}, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                base,
                mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
            );
            Some(Self { mac: Mac(mac), base })
        }
    }
}

struct NetState {
    ip: Ipv4,
    device: Option<virtio_net::VirtioNet>,
}

static NET_STATE: IrqSpinLock<NetState> = IrqSpinLock::new(NetState {
    ip: Ipv4([10, 0, 2, 15]),
    device: None,
});

/// Probe only the fixed VirtIO-MMIO aperture of QEMU's virt machine.
pub fn probe_qemu_virtio_mmio() {
    let mut base = 0x0a00_0000u64;
    for _ in 0..32 {
        if let Some(device) = virtio_net::VirtioNet::probe_qemu_slot(base) {
            NET_STATE.lock().device = Some(device);
            return;
        }
        base += 0x200;
    }
    crate::println!("Net: no QEMU VirtIO-MMIO network device found");
}

pub fn set_ip_address(ip: Ipv4) {
    NET_STATE.lock().ip = ip;
}

pub fn ip_address() -> Ipv4 {
    NET_STATE.lock().ip
}

pub fn mac_address() -> Option<Mac> {
    NET_STATE.lock().device.map(|device| device.mac)
}

pub fn report() {
    let state = NET_STATE.lock();
    if let Some(device) = state.device {
        crate::println!(
            "Net: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} IP {}.{}.{}.{}",
            device.mac.0[0], device.mac.0[1], device.mac.0[2],
            device.mac.0[3], device.mac.0[4], device.mac.0[5],
            state.ip.0[0], state.ip.0[1], state.ip.0[2], state.ip.0[3]
        );
    } else {
        crate::println!(
            "Net: no network device; configured IP {}.{}.{}.{}",
            state.ip.0[0], state.ip.0[1], state.ip.0[2], state.ip.0[3]
        );
    }
}
