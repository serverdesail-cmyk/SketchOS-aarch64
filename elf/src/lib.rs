//! Parser for the static AArch64 user images SketchOS 0.0.5 can load.
//!
//! The user ABI is deliberately tiny. A loadable image must be a
//! little-endian ELF64 `ET_EXEC` file for `EM_AARCH64` with one or more
//! `PT_LOAD` segments. Dynamic linking, an interpreter, thread-local storage
//! and writable-executable segments are rejected.
//!
//! Parsing never allocates and never trusts a file offset. The kernel walks the
//! returned segments, allocates physical pages, and copies `filesz` bytes into
//! each; the remaining `memsz - filesz` bytes are zeroed BSS.
//!
//! ```
//! # use sketchos_elf::{Image, MAX_SEGMENTS};
//! # fn bytes() -> &'static [u8] { &[] }
//! // A real image is embedded in the kernel with `include_bytes!`.
//! let parsed = Image::parse(bytes());
//! # let _ = parsed;
//! ```

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

/// Loadable segments kept per image. The kernel's user-image limit is far
/// smaller than this; the array only bounds hostile input.
pub const MAX_SEGMENTS: usize = 8;

const ELF_HEADER_SIZE: usize = 64;
const PROGRAM_HEADER_SIZE: usize = 56;

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const PT_TLS: u32 = 7;

const ET_EXEC: u16 = 2;
const EM_AARCH64: u16 = 183;

const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

/// One `PT_LOAD` segment, with addresses relative to the ELF's link base.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    /// Virtual address the segment is linked at.
    pub virtual_address: u64,
    /// File offset of the segment's initial bytes.
    pub offset: u64,
    /// Bytes present in the file.
    pub file_size: u64,
    /// Bytes present in memory, including zero-filled BSS.
    pub memory_size: u64,
    /// ELF `p_flags`: a combination of `PF_R`, `PF_W` and `PF_X`.
    pub flags: u32,
}

impl Segment {
    pub const fn readable(&self) -> bool {
        self.flags & PF_R != 0
    }

    pub const fn writable(&self) -> bool {
        self.flags & PF_W != 0
    }

    pub const fn executable(&self) -> bool {
        self.flags & PF_X != 0
    }

    /// One past the last virtual address the segment occupies.
    pub fn end(&self) -> u64 {
        self.virtual_address + self.memory_size
    }
}

/// A validated loadable image.
#[derive(Clone, Copy, Debug)]
pub struct Image {
    entry: u64,
    segments: [Segment; MAX_SEGMENTS],
    count: usize,
}

impl Image {
    /// Parse and validate an ELF image.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < ELF_HEADER_SIZE {
            return Err(Error::Truncated);
        }
        if bytes[0..4] != [0x7f, b'E', b'L', b'F'] {
            return Err(Error::BadMagic);
        }
        if bytes[4] != 2 {
            return Err(Error::NotElf64);
        }
        if bytes[5] != 1 {
            return Err(Error::NotLittleEndian);
        }
        if bytes[6] != 1 {
            return Err(Error::UnsupportedVersion);
        }

        let file_type = read_u16(bytes, 16)?;
        if file_type != ET_EXEC {
            return Err(Error::NotExecutable);
        }
        let machine = read_u16(bytes, 18)?;
        if machine != EM_AARCH64 {
            return Err(Error::NotAarch64);
        }
        if read_u32(bytes, 20)? != 1 {
            return Err(Error::UnsupportedVersion);
        }

        let entry = read_u64(bytes, 24)?;
        let program_offset = read_u64(bytes, 32)?;
        let header_size = read_u16(bytes, 52)? as usize;
        let entry_size = read_u16(bytes, 54)? as usize;
        let entry_count = read_u16(bytes, 56)? as usize;
        if header_size != ELF_HEADER_SIZE || entry_size != PROGRAM_HEADER_SIZE {
            return Err(Error::BadHeaderLayout);
        }
        if entry % 4 != 0 {
            return Err(Error::BadEntry);
        }

        let table_bytes = entry_count
            .checked_mul(PROGRAM_HEADER_SIZE)
            .ok_or(Error::Truncated)?;
        let table_end = program_offset
            .checked_add(table_bytes as u64)
            .ok_or(Error::Truncated)?;
        if table_end > bytes.len() as u64 {
            return Err(Error::Truncated);
        }

        let mut segments = [Segment {
            virtual_address: 0,
            offset: 0,
            file_size: 0,
            memory_size: 0,
            flags: 0,
        }; MAX_SEGMENTS];
        let mut count = 0usize;
        let mut entry_inside_text = false;

        for index in 0..entry_count {
            let base = program_offset as usize + index * PROGRAM_HEADER_SIZE;
            let kind = read_u32(bytes, base)?;
            let flags = read_u32(bytes, base + 4)?;
            let offset = read_u64(bytes, base + 8)?;
            let virtual_address = read_u64(bytes, base + 16)?;
            let file_size = read_u64(bytes, base + 32)?;
            let memory_size = read_u64(bytes, base + 40)?;
            let alignment = read_u64(bytes, base + 48)?;

            match kind {
                PT_LOAD => {}
                PT_DYNAMIC => return Err(Error::DynamicLinking),
                PT_INTERP => return Err(Error::Interpreter),
                PT_TLS => return Err(Error::ThreadLocalStorage),
                _ => continue,
            }

            if file_size > memory_size {
                return Err(Error::BadSegment);
            }
            if memory_size == 0 {
                return Err(Error::BadSegment);
            }
            if flags & PF_X != 0 && flags & PF_W != 0 {
                return Err(Error::WritableExecutable);
            }
            if virtual_address % 4096 != offset % 4096 {
                return Err(Error::MisalignedSegment);
            }
            if alignment > 1 {
                if !alignment.is_power_of_two() {
                    return Err(Error::BadAlignment);
                }
                if virtual_address % alignment != offset % alignment {
                    return Err(Error::MisalignedSegment);
                }
            }
            let file_end = offset.checked_add(file_size).ok_or(Error::Truncated)?;
            if file_end > bytes.len() as u64 {
                return Err(Error::Truncated);
            }
            let memory_end = virtual_address
                .checked_add(memory_size)
                .ok_or(Error::BadSegment)?;
            if memory_end > (1u64 << 48) {
                return Err(Error::BadSegment);
            }

            if flags & PF_X != 0
                && entry >= virtual_address
                && entry < memory_end
            {
                entry_inside_text = true;
            }

            if count == MAX_SEGMENTS {
                return Err(Error::TooManySegments);
            }
            segments[count] = Segment {
                virtual_address,
                offset,
                file_size,
                memory_size,
                flags,
            };
            count += 1;
        }

        if count == 0 {
            return Err(Error::NoLoadableSegments);
        }
        if !entry_inside_text {
            return Err(Error::BadEntry);
        }

        Ok(Self {
            entry,
            segments,
            count,
        })
    }

    /// EL0 entry address.
    pub const fn entry(&self) -> u64 {
        self.entry
    }

    /// Validated loadable segments.
    pub fn segments(&self) -> &[Segment] {
        &self.segments[..self.count]
    }
}

/// Why an image was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Truncated,
    BadMagic,
    NotElf64,
    NotLittleEndian,
    UnsupportedVersion,
    NotExecutable,
    NotAarch64,
    BadHeaderLayout,
    BadEntry,
    TruncatedProgramHeader,
    TooManySegments,
    Interpreter,
    DynamicLinking,
    ThreadLocalStorage,
    WritableExecutable,
    MisalignedSegment,
    BadAlignment,
    BadSegment,
    NoLoadableSegments,
}

impl Error {
    pub const fn message(self) -> &'static str {
        match self {
            Error::Truncated => "ELF file is truncated",
            Error::BadMagic => "not an ELF file",
            Error::NotElf64 => "not a 64-bit ELF file",
            Error::NotLittleEndian => "not a little-endian ELF file",
            Error::UnsupportedVersion => "unsupported ELF version",
            Error::NotExecutable => "not an ET_EXEC image",
            Error::NotAarch64 => "not an AArch64 image",
            Error::BadHeaderLayout => "unexpected ELF header size",
            Error::BadEntry => "entry point is not inside executable code",
            Error::TruncatedProgramHeader => "program header table is truncated",
            Error::TooManySegments => "too many loadable segments",
            Error::Interpreter => "interpreter segments are not supported",
            Error::DynamicLinking => "dynamic linking is not supported",
            Error::ThreadLocalStorage => "thread-local storage is not supported",
            Error::WritableExecutable => "a segment is writable and executable",
            Error::MisalignedSegment => "segment file and memory alignment disagree",
            Error::BadAlignment => "segment alignment is not a power of two",
            Error::BadSegment => "segment has an invalid size or address",
            Error::NoLoadableSegments => "image has no loadable segments",
        }
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, Error> {
    let slice = bytes
        .get(offset..offset + 2)
        .ok_or(Error::TruncatedProgramHeader)?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, Error> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or(Error::TruncatedProgramHeader)?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, Error> {
    let slice = bytes
        .get(offset..offset + 8)
        .ok_or(Error::TruncatedProgramHeader)?;
    Ok(u64::from_le_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    fn header(file_type: u16, machine: u16, entry: u64) -> Vec<u8> {
        let mut bytes = vec![0u8; ELF_HEADER_SIZE];
        bytes[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[16..18].copy_from_slice(&file_type.to_le_bytes());
        bytes[18..20].copy_from_slice(&machine.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        bytes[24..32].copy_from_slice(&entry.to_le_bytes());
        bytes[32..40].copy_from_slice(&(ELF_HEADER_SIZE as u64).to_le_bytes());
        bytes[52..54].copy_from_slice(&(ELF_HEADER_SIZE as u16).to_le_bytes());
        bytes[54..56].copy_from_slice(&(PROGRAM_HEADER_SIZE as u16).to_le_bytes());
        bytes
    }

    fn program_header(
        bytes: &mut Vec<u8>,
        kind: u32,
        flags: u32,
        offset: u64,
        vaddr: u64,
        file_size: u64,
        memory_size: u64,
        alignment: u64,
    ) {
        let mut entry = vec![0u8; PROGRAM_HEADER_SIZE];
        entry[0..4].copy_from_slice(&kind.to_le_bytes());
        entry[4..8].copy_from_slice(&flags.to_le_bytes());
        entry[8..16].copy_from_slice(&offset.to_le_bytes());
        entry[16..24].copy_from_slice(&vaddr.to_le_bytes());
        entry[32..40].copy_from_slice(&file_size.to_le_bytes());
        entry[40..48].copy_from_slice(&memory_size.to_le_bytes());
        entry[48..56].copy_from_slice(&alignment.to_le_bytes());
        bytes.extend_from_slice(&entry);
    }

    fn image() -> Vec<u8> {
        let mut bytes = header(ET_EXEC, EM_AARCH64, 0x40_0000);
        bytes[56..58].copy_from_slice(&1u16.to_le_bytes());
        program_header(&mut bytes, PT_LOAD, PF_R | PF_X, 0x1000, 0x40_0000, 4, 8, 4096);
        bytes.resize(0x1004, 0xaa);
        bytes
    }

    #[test]
    fn parses_a_minimal_image() {
        let image = Image::parse(&image()).unwrap();
        assert_eq!(image.entry(), 0x40_0000);
        assert_eq!(image.segments().len(), 1);
        let segment = image.segments()[0];
        assert!(segment.readable());
        assert!(!segment.writable());
        assert!(segment.executable());
        assert_eq!(segment.end(), 0x40_0008);
    }

    #[test]
    fn rejects_dynamic_linking_and_interpreters() {
        let mut dynamic = header(ET_EXEC, EM_AARCH64, 0x40_0000);
        dynamic[56..58].copy_from_slice(&1u16.to_le_bytes());
        program_header(&mut dynamic, PT_DYNAMIC, PF_R, 0, 0, 0, 0, 0);
        assert_eq!(Image::parse(&dynamic).unwrap_err(), Error::DynamicLinking);

        let mut interp = header(ET_EXEC, EM_AARCH64, 0x40_0000);
        interp[56..58].copy_from_slice(&1u16.to_le_bytes());
        program_header(&mut interp, PT_INTERP, PF_R, 0, 0, 0, 0, 0);
        assert_eq!(Image::parse(&interp).unwrap_err(), Error::Interpreter);
    }

    #[test]
    fn rejects_writable_executable_segments() {
        let mut bytes = header(ET_EXEC, EM_AARCH64, 0x40_0000);
        bytes[56..58].copy_from_slice(&1u16.to_le_bytes());
        program_header(
            &mut bytes,
            PT_LOAD,
            PF_R | PF_W | PF_X,
            0x1000,
            0x40_0000,
            4,
            8,
            4096,
        );
        bytes.resize(0x1004, 0);
        assert_eq!(
            Image::parse(&bytes).unwrap_err(),
            Error::WritableExecutable
        );
    }

    #[test]
    fn rejects_a_non_executable_entry() {
        let mut bytes = header(ET_EXEC, EM_AARCH64, 0x80_0000);
        bytes[56..58].copy_from_slice(&1u16.to_le_bytes());
        program_header(&mut bytes, PT_LOAD, PF_R | PF_X, 0x1000, 0x40_0000, 4, 8, 4096);
        bytes.resize(0x1004, 0);
        assert_eq!(Image::parse(&bytes).unwrap_err(), Error::BadEntry);
    }

    #[test]
    fn rejects_misaligned_and_truncated_segments() {
        let mut misaligned = header(ET_EXEC, EM_AARCH64, 0x40_0000);
        misaligned[56..58].copy_from_slice(&1u16.to_le_bytes());
        program_header(
            &mut misaligned,
            PT_LOAD,
            PF_R | PF_X,
            0x1001,
            0x40_0000,
            4,
            8,
            4096,
        );
        misaligned.resize(0x1004, 0);
        assert_eq!(
            Image::parse(&misaligned).unwrap_err(),
            Error::MisalignedSegment
        );

        let mut truncated = header(ET_EXEC, EM_AARCH64, 0x40_0000);
        truncated[56..58].copy_from_slice(&1u16.to_le_bytes());
        program_header(
            &mut truncated,
            PT_LOAD,
            PF_R | PF_X,
            0x1000,
            0x40_0000,
            0x100,
            0x100,
            4096,
        );
        assert_eq!(Image::parse(&truncated).unwrap_err(), Error::Truncated);
    }

    #[test]
    fn rejects_wrong_machine_and_type() {
        let mut dynamic = image();
        dynamic[16..18].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(Image::parse(&dynamic).unwrap_err(), Error::NotExecutable);

        let mut wrong = image();
        wrong[18..20].copy_from_slice(&62u16.to_le_bytes());
        assert_eq!(Image::parse(&wrong).unwrap_err(), Error::NotAarch64);
    }
}
