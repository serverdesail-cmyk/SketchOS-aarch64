//! DBG2 (Debug Port Table 2) decoding.
//!
//! The laptop firmware describes seven debug devices: one UART at
//! `\_SB.UARD` (`0x894000`, subtype `0x13`) and three high-speed serial
//! instances. The offsets below are the Microsoft-documented layout and were
//! cross-checked against the firmware dump:
//!
//! ```text
//! 0   Revision                      1
//! 1   Length                        2
//! 3   NumberOfGenericAddressRegs    1
//! 4   NamespaceStringLength         2
//! 6   NamespaceStringOffset         2
//! 8   OemDataLength                 2
//! 10  OemDataOffset                 2
//! 12  PortType                      2
//! 14  PortSubtype                   2
//! 16  Reserved                      2
//! 18  BaseAddressRegisterOffset     2
//! 20  AddressSizeOffset             2
//! ```

use super::reader::Table;
use super::{Dbg2Port, Platform, MAX_DBG2_PORTS};

const HEADER: usize = 36;

pub fn parse(table: &Table<'_>, platform: &mut Platform) {
    let mut offset = table.u32(36) as usize;
    let count = table.u32(40);
    if offset == 0 {
        offset = HEADER + 8;
    }
    let length = table.length();

    // The declared count is untrusted; the table length is the real limit.
    let mut entry = 0u32;
    while entry < count && offset + 22 <= length {
        let entry_length = table.u16(offset + 1) as usize;
        if entry_length < 22 || offset.saturating_add(entry_length) > length {
            break;
        }

        let namespace_length = table.u16(offset + 4).min(15) as usize;
        let namespace_offset = table.u16(offset + 6) as usize;
        let gas_offset = table.u16(offset + 18) as usize;

        let mut namespace = [0u8; 16];
        for index in 0..namespace_length {
            namespace[index] = table.u8(offset + namespace_offset + index);
        }

        // A Generic Address Structure is 12 bytes; the address itself is the
        // second 8-byte field. Reading through the table accessor keeps this
        // bounds-checked even when the offset is corrupt.
        let base = if gas_offset + 12 <= entry_length {
            table.u64(offset + gas_offset + 4)
        } else {
            0
        };

        if platform.dbg2_count < MAX_DBG2_PORTS {
            platform.dbg2[platform.dbg2_count] = Dbg2Port {
                namespace,
                namespace_length: namespace_length as u8,
                port_type: table.u16(offset + 12),
                subtype: table.u16(offset + 14),
                base,
            };
            platform.dbg2_count += 1;
        }
        platform.dbg2_ports += 1;

        offset += entry_length;
        entry += 1;
    }
}
