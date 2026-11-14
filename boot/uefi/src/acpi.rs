//! Minimal ACPI discovery for the loader.
//!
//! The RSDP is taken from the UEFI configuration table (never from a BIOS
//! memory scan), validated with its checksums, and used to walk either the
//! XSDT (ACPI 2.0+) or RSDT. The walk collects the table count and the SPCR
//! serial description.

use sketchos_bootinfo::uart_kind;
use uefi::system;
use uefi::table::cfg::ConfigTableEntry;

#[derive(Clone, Copy)]
pub struct AcpiDiscovery {
    pub rsdp_address: u64,
    pub revision: u8,
    pub oem_id: [u8; 6],
    pub xsdt_address: u64,
    pub table_count: usize,
    pub checksum_ok: bool,
    pub spcr: Option<Spcr>,
    pub qemu: bool,
}

#[derive(Clone, Copy)]
pub struct Spcr {
    pub base: u64,
    pub interface: u8,
    pub kind: u32,
}

pub fn discover() -> Option<AcpiDiscovery> {
    let rsdp = system::with_config_table(|entries| {
        let mut acpi1 = None;
        for entry in entries {
            if entry.guid == ConfigTableEntry::ACPI2_GUID {
                return Some(entry.address as *const u8);
            }
            if entry.guid == ConfigTableEntry::ACPI_GUID {
                acpi1 = Some(entry.address as *const u8);
            }
        }
        acpi1
    })?;

    unsafe {
        let parsed = parse(rsdp);
        match &parsed {
            Some(a) => {
                uefi::println!(
                    "ACPI RSDP: 0x{:016x} rev {} OEM \"{}\" XSDT 0x{:016x}",
                    a.rsdp_address,
                    a.revision,
                    ascii(&a.oem_id),
                    a.xsdt_address
                );
                uefi::println!(
                    "ACPI tables: {} entries, checksum {}",
                    a.table_count,
                    if a.checksum_ok { "ok" } else { "BAD" }
                );
                if let Some(spcr) = &a.spcr {
                    uefi::println!(
                        "ACPI SPCR: interface 0x{:02x} base 0x{:016x} ({})",
                        spcr.interface,
                        spcr.base,
                        uart_kind::name(spcr.kind)
                    );
                } else {
                    uefi::println!("ACPI SPCR: not found");
                }
            }
            None => uefi::println!("ACPI: no valid RSDP found"),
        }
        parsed
    }
}

unsafe fn parse(rsdp: *const u8) -> Option<AcpiDiscovery> {
    if !valid_rsdp(rsdp) {
        return None;
    }

    let revision = read_u8(rsdp, 15);
    let mut oem_id = [0u8; 6];
    for (i, byte) in oem_id.iter_mut().enumerate() {
        *byte = read_u8(rsdp, 9 + i);
    }
    let qemu = oem_id == *b"BOCHS ";

    let mut checksum_ok = true;
    let mut sdt_address = 0u64;
    let mut table_count = 0usize;
    let mut spcr = None;

    if revision >= 2 {
        let length = read_u32(rsdp, 20) as usize;
        checksum_ok = checksum(rsdp, length) == 0;

        let xsdt = read_u64(rsdp, 24);
        if xsdt != 0 {
            sdt_address = xsdt;
            let (count, table_sum, table_spcr) = walk_sdt(xsdt as *const u8, b"XSDT");
            if count == 0 {
                checksum_ok = false;
            }
            table_count = count;
            checksum_ok &= table_sum;
            spcr = table_spcr;
        }
    } else {
        let rsdt = read_u32(rsdp, 16) as u64;
        if rsdt != 0 {
            sdt_address = rsdt;
            let (count, table_sum, table_spcr) = walk_sdt(rsdt as *const u8, b"RSDT");
            table_count = count;
            checksum_ok &= table_sum;
            spcr = table_spcr;
        }
    }

    Some(AcpiDiscovery {
        rsdp_address: rsdp as u64,
        revision,
        oem_id,
        xsdt_address: sdt_address,
        table_count,
        checksum_ok,
        spcr,
        qemu,
    })
}

unsafe fn walk_sdt(
    sdt: *const u8,
    expected: &[u8; 4],
) -> (usize, bool, Option<Spcr>) {
    if read_bytes(sdt, 0) != *expected {
        return (0, false, None);
    }

    let length = read_u32(sdt, 4) as usize;
    if !(36..=0x10_0000).contains(&length) {
        return (0, false, None);
    }
    let checksum_ok = checksum(sdt, length) == 0;
    let count = (length - 36) / if *expected == *b"XSDT" { 8 } else { 4 };
    let mut spcr = None;

    for i in 0..count {
        let entry = if *expected == *b"XSDT" {
            read_u64(sdt, 36 + i * 8)
        } else {
            read_u32(sdt, 36 + i * 4) as u64
        };
        if entry == 0 {
            continue;
        }
        let table = entry as *const u8;
        if read_bytes(table, 0) == *b"SPCR" {
            spcr = parse_spcr(table);
        }
    }

    (count, checksum_ok, spcr)
}

unsafe fn parse_spcr(table: *const u8) -> Option<Spcr> {
    let length = read_u32(table, 4) as usize;
    if !(80..=4096).contains(&length)
        || read_bytes(table, 0) != *b"SPCR"
        || checksum(table, length) != 0
    {
        return None;
    }

    let interface = read_u8(table, 36);
    // Generic Address Structure: space ID, bit width, bit offset, access size,
    // then the 64-bit register base.
    let gas_space = read_u8(table, 40);
    let gas_width = read_u8(table, 41);
    let gas_offset = read_u8(table, 42);
    let gas_access_size = read_u8(table, 43);
    let raw_base = read_u64(table, 44);

    // GENI and PL011 are 32-bit SystemMemory register blocks. Firmware may
    // leave width/access-size zero to mean the interface-defined default.
    let gas_valid = gas_space == 0
        && gas_offset == 0
        && (gas_width == 0 || gas_width == 32)
        && (gas_access_size == 0 || gas_access_size == 3);
    let base_valid = raw_base != 0
        && raw_base & 0x3 == 0
        && raw_base
            .checked_add(4096)
            .is_some_and(|end| end <= (1u64 << 39));

    let kind = if gas_valid && base_valid {
        match interface {
            0x00..=0x02 => uart_kind::UART16550,
            0x03 => uart_kind::PL011,
            0x0e => uart_kind::SBSA,
            0x13 => uart_kind::SDM845_GENI,
            _ => uart_kind::OTHER,
        }
    } else {
        uart_kind::OTHER
    };

    Some(Spcr {
        base: if gas_valid && base_valid { raw_base } else { 0 },
        interface,
        kind,
    })
}

unsafe fn valid_rsdp(rsdp: *const u8) -> bool {
    if read_bytes(rsdp, 0) != *b"RSD " || read_bytes(rsdp, 4) != *b"PTR " {
        return false;
    }
    if checksum(rsdp, 20) != 0 {
        return false;
    }
    let revision = read_u8(rsdp, 15);
    if revision >= 2 {
        let length = read_u32(rsdp, 20) as usize;
        if !(36..=4096).contains(&length) {
            return false;
        }
        if checksum(rsdp, length) != 0 {
            return false;
        }
        if read_u64(rsdp, 24) == 0 {
            return false;
        }
    }
    true
}

unsafe fn checksum(base: *const u8, length: usize) -> u8 {
    let mut sum = 0u8;
    for i in 0..length {
        sum = sum.wrapping_add(read_u8(base, i));
    }
    sum
}

unsafe fn read_bytes(base: *const u8, offset: usize) -> [u8; 4] {
    let mut out = [0u8; 4];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = core::ptr::read_volatile(base.add(offset + i));
    }
    out
}

unsafe fn read_u8(base: *const u8, offset: usize) -> u8 {
    core::ptr::read_volatile(base.add(offset))
}

unsafe fn read_u32(base: *const u8, offset: usize) -> u32 {
    u32::from_le(core::ptr::read_unaligned(
        base.add(offset).cast::<u32>(),
    ))
}

unsafe fn read_u64(base: *const u8, offset: usize) -> u64 {
    u64::from_le(core::ptr::read_unaligned(
        base.add(offset).cast::<u64>(),
    ))
}

fn ascii(bytes: &[u8]) -> &str {
    core::str::from_utf8(bytes).unwrap_or("??????")
}
