//! Quiesce PCI bus mastering before the kernel takes ownership of RAM.
//!
//! Linux's arm64 boot protocol requires DMA-capable devices to be quiesced
//! before entering the kernel. The UEFI loader has finished all filesystem and
//! firmware I/O by the time this module is called, so clearing the PCI Command
//! register's Bus Master Enable bit is safe for the handoff.

use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams};
use uefi::proto::pci::root_bridge::PciRootBridgeIo;

const PCI_COMMAND: u32 = 0x04;
const PCI_COMMAND_BUS_MASTER: u16 = 1 << 2;

/// Clear Bus Master Enable on every PCI function reachable through UEFI root
/// bridges. Failures are reported but do not prevent boot: some platforms do
/// not expose PCI root-bridge protocols or contain non-PCI DMA engines.
pub fn quiesce_pci_bus_masters() {
    let handles = match boot::find_handles::<PciRootBridgeIo>() {
        Ok(handles) => handles,
        Err(_) => {
            uefi::println!("PCI DMA: no UEFI PCI root bridges exposed");
            return;
        }
    };

    let mut devices = 0usize;
    let mut disabled = 0usize;
    let mut failures = 0usize;

    for handle in handles {
        let params = OpenProtocolParams {
            handle,
            agent: boot::image_handle(),
            controller: None,
        };

        // SAFETY: GetProtocol borrows the firmware-owned root-bridge protocol
        // for this call. It is dropped before ExitBootServices.
        let mut root = match unsafe {
            boot::open_protocol::<PciRootBridgeIo>(params, OpenProtocolAttributes::GetProtocol)
        } {
            Ok(root) => root,
            Err(_) => {
                failures += 1;
                continue;
            }
        };

        let tree = match root.enumerate() {
            Ok(tree) => tree,
            Err(_) => {
                failures += 1;
                continue;
            }
        };

        let pci = root.pci();
        for address in tree.iter() {
            devices += 1;
            let command_address = address.with_register(PCI_COMMAND);
            let command = match pci.read_one::<u16>(command_address) {
                Ok(command) => command,
                Err(_) => {
                    failures += 1;
                    continue;
                }
            };

            if command & PCI_COMMAND_BUS_MASTER == 0 {
                continue;
            }

            match pci.write_one::<u16>(
                command_address,
                command & !PCI_COMMAND_BUS_MASTER,
            ) {
                Ok(()) => disabled += 1,
                Err(_) => failures += 1,
            }
        }
    }

    uefi::println!(
        "PCI DMA: {} functions scanned, {} bus masters disabled, {} errors",
        devices,
        disabled,
        failures
    );
}
