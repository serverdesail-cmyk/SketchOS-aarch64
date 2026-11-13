//! Small table decoders: SPCR, BGRT and TPM2.

use super::reader::Table;
use super::{Bgrt, Platform, SpcrInfo, Tpm2};

const HEADER: usize = 36;

pub fn parse_spcr(table: &Table<'_>, platform: &mut Platform) {
    // SPCR: interface type at 36, then a Generic Address Structure at 40
    // whose 64-bit address field starts at 44.
    platform.spcr = Some(SpcrInfo {
        interface: table.u8(HEADER),
        base: table.u64(HEADER + 8),
        interrupt_type: table.u8(HEADER + 16),
        gsiv: table.u32(HEADER + 18),
        baud: table.u8(HEADER + 22),
        parity: table.u8(HEADER + 23),
        stop_bits: table.u8(HEADER + 24),
        flow_control: table.u8(HEADER + 25),
    });
}

pub fn parse_bgrt(table: &Table<'_>, platform: &mut Platform) {
    // BGRT has no width or height field. The image's own header carries its
    // dimensions; the kernel records only the table's own data.
    platform.bgrt = Some(Bgrt {
        version: table.u16(HEADER),
        status: table.u8(HEADER + 2),
        image_type: table.u8(HEADER + 3),
        address: table.u64(HEADER + 4),
        offset_x: table.u32(HEADER + 12),
        offset_y: table.u32(HEADER + 16),
    });
}

pub fn parse_tpm2(table: &Table<'_>, platform: &mut Platform) {
    platform.tpm2 = Some(Tpm2 {
        platform_class: table.u16(HEADER),
        control_area: table.u64(HEADER + 4),
        start_method: table.u32(HEADER + 12),
    });
}
