//! Safe, bounds-checked views over firmware ACPI tables.
//!
//! A [`Table`] validates the 36-byte header and the declared length before
//! exposing a byte slice. Every field accessor is bounds-checked and returns
//! zero (or `None`) for a truncated table instead of reading past the end.
//!
//! The underlying memory is firmware-owned and remains valid for the life of
//! the boot. Creating a table from a raw address is therefore the only unsafe
//! step, and it is confined to [`Table::from_raw`] and [`Rsdp::parse`]. Both
//! accept *physical* addresses and translate them through the linear map; the
//! [`Rsdp::address`] field stays physical for reporting.

use sketchos_bootinfo::phys_to_virt;

/// Largest table this kernel will look at. Firmware tables are small; the cap
/// prevents a corrupt length field from causing an enormous slice.
pub const MAX_TABLE_LENGTH: usize = 0x10_0000;
/// Current kernel physmap covers the first 512 GiB of physical address space.
pub const MAX_PHYS_MAP_BYTES: u64 = 512u64 << 30;

/// A validated ACPI system description table.
#[derive(Clone, Copy)]
pub struct Table<'a> {
    data: &'a [u8],
}

// A few accessors are part of the reader's public surface for upcoming table
// decoders but are not called by milestone 0.0.2.
#[allow(dead_code)]
impl<'a> Table<'a> {
    /// Build a table view from a physical address supplied by firmware.
    ///
    /// # Safety
    ///
    /// `address` must point to readable firmware memory containing a complete
    /// ACPI table header. The kernel trusts the firmware's table list, but it
    /// still validates signature, length and checksum before interpreting any
    /// payload.
    pub unsafe fn from_raw(address: u64) -> Option<Self> {
        if address == 0
            || address % 4 != 0
            || address
                .checked_add(36)
                .map_or(true, |end| end > MAX_PHYS_MAP_BYTES)
        {
            return None;
        }
        let ptr = phys_to_virt(address) as *const u8;

        // SAFETY: the caller guarantees the header is readable.
        let signature = unsafe { core::ptr::read_unaligned(ptr.cast::<[u8; 4]>()) };
        if !signature.iter().all(|byte| byte.is_ascii_graphic()) {
            return None;
        }

        // SAFETY: as above; the length field is part of the 36-byte header.
        let length =
            u32::from_le(unsafe { core::ptr::read_unaligned(ptr.add(4).cast::<u32>()) }) as usize;
        if !(36..=MAX_TABLE_LENGTH).contains(&length)
            || address
                .checked_add(length as u64)
                .map_or(true, |end| end > MAX_PHYS_MAP_BYTES)
        {
            return None;
        }

        // SAFETY: the header advertised `length` bytes of a firmware table.
        let data = unsafe { core::slice::from_raw_parts(ptr, length) };
        Some(Self { data })
    }

    /// Raw table bytes, including the 36-byte header.
    pub fn bytes(&self) -> &'a [u8] {
        self.data
    }

    pub fn signature(&self) -> [u8; 4] {
        [self.data[0], self.data[1], self.data[2], self.data[3]]
    }

    pub fn signature_str(&self) -> &'a str {
        core::str::from_utf8(&self.data[0..4]).unwrap_or("????")
    }

    pub fn is(&self, signature: &[u8; 4]) -> bool {
        self.data[0..4] == signature[..]
    }

    pub fn length(&self) -> usize {
        self.data.len()
    }

    pub fn revision(&self) -> u8 {
        self.data[8]
    }

    pub fn checksum(&self) -> u8 {
        self.data.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte))
    }

    pub fn checksum_ok(&self) -> bool {
        self.checksum() == 0
    }

    pub fn oem_id(&self) -> &'a [u8] {
        &self.data[10..16]
    }

    pub fn oem_table_id(&self) -> &'a [u8] {
        &self.data[16..24]
    }

    pub fn creator_id(&self) -> &'a [u8] {
        &self.data[28..32]
    }

    /// Bounds-checked byte access. Returns zero for a truncated field.
    pub fn u8(&self, offset: usize) -> u8 {
        self.data.get(offset).copied().unwrap_or(0)
    }

    /// Bounds-checked little-endian 16-bit access.
    pub fn u16(&self, offset: usize) -> u16 {
        self.data
            .get(offset..offset + 2)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u16::from_le_bytes)
            .unwrap_or(0)
    }

    /// Bounds-checked little-endian 32-bit access.
    pub fn u32(&self, offset: usize) -> u32 {
        self.data
            .get(offset..offset + 4)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_le_bytes)
            .unwrap_or(0)
    }

    /// Bounds-checked little-endian 64-bit access.
    pub fn u64(&self, offset: usize) -> u64 {
        self.data
            .get(offset..offset + 8)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u64::from_le_bytes)
            .unwrap_or(0)
    }

    /// Bounds-checked slice access.
    pub fn slice(&self, offset: usize, length: usize) -> &'a [u8] {
        let end = offset.saturating_add(length);
        self.data.get(offset..end).unwrap_or(&[])
    }

    pub fn has(&self, offset: usize, length: usize) -> bool {
        offset
            .checked_add(length)
            .is_some_and(|end| end <= self.data.len())
    }
}

/// Validated Root System Description Pointer.
#[derive(Clone, Copy)]
pub struct Rsdp {
    pub address: u64,
    pub revision: u8,
    pub oem_id: [u8; 6],
    pub root_address: u64,
    pub root_is_xsdt: bool,
    /// Both the ACPI 1.0 checksum and (for revision 2+) the extended checksum
    /// passed.
    pub checksum_ok: bool,
}

impl Rsdp {
    /// Parse an RSDP from a firmware-provided physical address.
    ///
    /// # Safety
    ///
    /// `address` must point to readable firmware memory containing an RSDP.
    pub unsafe fn parse(address: u64) -> Option<Self> {
        // ACPI recommends 16-byte alignment, but firmware is not required to
        // honour it and QEMU/EDK2 hands off an 8-byte-aligned RSDP. All reads
        // below are unaligned-safe, so only a null address is rejected.
        if address == 0
            || address
                .checked_add(36)
                .map_or(true, |end| end > MAX_PHYS_MAP_BYTES)
        {
            return None;
        }
        let ptr = phys_to_virt(address) as *const u8;

        // SAFETY: the caller guarantees the RSDP is readable.
        let signature = unsafe { core::ptr::read_unaligned(ptr.cast::<[u8; 8]>()) };
        if signature != *b"RSD PTR " {
            return None;
        }

        // SAFETY: signature matched; the remaining fields are in the first 36
        // bytes of the structure.
        let revision = unsafe { core::ptr::read_volatile(ptr.add(15)) };
        let length = if revision >= 2 {
            u32::from_le(unsafe {
                core::ptr::read_unaligned(ptr.add(20).cast::<u32>())
            }) as usize
        } else {
            20
        };
        if !(20..=4096).contains(&length)
            || address
                .checked_add(length as u64)
                .map_or(true, |end| end > MAX_PHYS_MAP_BYTES)
        {
            return None;
        }
        if revision >= 2 && length < 36 {
            return None;
        }

        let mut checksum_ok = unsafe { checksum(ptr, 20) } == 0;
        if revision >= 2 {
            checksum_ok &= unsafe { checksum(ptr, length) } == 0;
        }

        let mut oem_id = [0u8; 6];
        for (index, byte) in oem_id.iter_mut().enumerate() {
            // SAFETY: OEM ID lies within the first 20 bytes.
            *byte = unsafe { core::ptr::read_volatile(ptr.add(9 + index)) };
        }

        let (root_address, root_is_xsdt) = if revision >= 2 {
            // SAFETY: revision 2+ RSDP includes the 64-bit XSDT address.
            (unsafe { core::ptr::read_unaligned(ptr.add(24).cast::<u64>()) }, true)
        } else {
            // SAFETY: ACPI 1.0 includes the 32-bit RSDT address.
            (
                u32::from_le(unsafe {
                    core::ptr::read_unaligned(ptr.add(16).cast::<u32>())
                }) as u64,
                false,
            )
        };

        Some(Self {
            address,
            revision,
            oem_id,
            root_address,
            root_is_xsdt,
            checksum_ok,
        })
    }
}

unsafe fn checksum(base: *const u8, length: usize) -> u8 {
    let mut sum = 0u8;
    for offset in 0..length {
        // SAFETY: the caller guarantees `length` readable bytes.
        sum = sum.wrapping_add(unsafe { core::ptr::read_volatile(base.add(offset)) });
    }
    sum
}

/// Render an ASCII identifier, stopping at the first NUL or non-graphic byte.
#[allow(dead_code)]
pub fn ascii(bytes: &[u8]) -> &str {
    let end = bytes
        .iter()
        .position(|&byte| byte == 0 || !byte.is_ascii_graphic())
        .unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..end]).unwrap_or("?")
}

/// Render bytes as space-padded ASCII, keeping interior spaces.
pub fn ascii_padded(bytes: &[u8]) -> &str {
    let end = bytes
        .iter()
        .rposition(|&byte| byte != b' ' && byte != 0)
        .map(|index| index + 1)
        .unwrap_or(0);
    core::str::from_utf8(&bytes[..end]).unwrap_or("?")
}
