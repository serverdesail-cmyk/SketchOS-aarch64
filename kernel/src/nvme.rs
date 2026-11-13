//! NVMe driver (milestone 0.0.8, scaffold only).
//!
//! A full NVMe driver requires: PCIe BAR sizing (pci.rs currently only
//! enumerates, it does not size/write BARs), admin submission/completion
//! queues, an I/O queue pair, PRP lists for multi-page transfers, and a
//! namespace identify. That is ~400 lines plus a BAR-mapping change to pci.rs,
//! and it cannot be validated here because the QEMU smoke test does not attach
//! an NVMe controller. The driver is therefore registered as a scaffold; the
//! block-device abstraction (storage.rs) is ready to accept it once implemented.

pub fn probe_and_report() {
    crate::println!("NVMe: driver scaffold registered (PCIe class 0x01/0x08); full implementation pending BAR sizing in pci.rs");
}
