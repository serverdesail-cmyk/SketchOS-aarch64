//! ACPI inventory and platform description.
//!
//! The loader validated the RSDP and handed its physical address to the
//! kernel. This module repeats the validation, walks the XSDT/RSDT, reports
//! every table it finds, and decodes the tables that describe the hardware
//! this milestone can use: interrupt controllers, timers, CPU topology,
//! PCIe ECAM windows, I/O remapping, NUMA affinity, debug ports and the
//! boot graphics image.
//!
//! Decoding is read-only. Nothing here programs a device, writes ACPI memory
//! or calls firmware.

mod dbg2;
mod facp;
mod gtdt;
mod iort;
mod madt;
mod mcfg;
mod misc;
pub mod pptt;
mod reader;
mod srat;

use crate::println;
use sketchos_bootinfo::{BootInfo, FLAG_RSDP_VALID};

use reader::{ascii_padded, Rsdp, Table};

use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

/// Maximum CPUs the boot description keeps. The laptop has 12 MADT entries
/// (8 enabled); 64 leaves room for larger servers without allocating.
pub const MAX_CPUS: usize = 64;
pub const MAX_GICR: usize = 32;
pub const MAX_ITS: usize = 8;
pub const MAX_MCFG: usize = 8;
pub const MAX_MEMORY_DOMAINS: usize = 32;
pub const MAX_DBG2_PORTS: usize = 8;
pub const MAX_PPTT_NODES: usize = 64;
pub const PPTT_CLUSTER_UNKNOWN: u16 = u16::MAX;

/// Aff0..Aff3 of MPIDR_EL1. Excludes the U bit and implementation-defined bits
/// that firmware does not record in MADT GICC entries.
pub const MPIDR_AFFINITY_MASK: u64 = 0x0000_00FF_00FF_FFFF;

/// One GIC CPU interface from MADT.
#[derive(Clone, Copy)]
pub struct Cpu {
    pub interface: u32,
    pub uid: u32,
    pub mpidr: u64,
    pub gicc: u64,
    pub gicv: u64,
    pub gich: u64,
    pub gicr: u64,
    pub performance_gsiv: u32,
    pub vgic_gsiv: u32,
    pub enabled: bool,
    /// 0 is the highest-performance class; larger values are more
    /// power-efficient, lower-performance classes.
    pub efficiency: u8,
}

impl Cpu {
    pub const EMPTY: Self = Self {
        interface: 0,
        uid: 0,
        mpidr: 0,
        gicc: 0,
        gicv: 0,
        gich: 0,
        gicr: 0,
        performance_gsiv: 0,
        vgic_gsiv: 0,
        enabled: false,
        efficiency: 0,
    };
}

/// One PPTT hierarchy node.
///
/// `parent` is the table-relative byte offset of the parent node; zero means
/// the root. `acpi_id` is only meaningful when `flags` bit 1 is set.
#[derive(Clone, Copy)]
pub struct PpttNode {
    pub offset: u32,
    pub kind: u8,
    pub flags: u8,
    pub parent: u32,
    pub acpi_id: u32,
}

impl PpttNode {
    pub const EMPTY: Self = Self {
        offset: 0,
        kind: 0,
        flags: 0,
        parent: 0,
        acpi_id: 0,
    };
}

/// GIC distributor.
#[derive(Clone, Copy)]
pub struct Gicd {
    pub base: u64,
    pub version: u8,
    pub id: u32,
}

/// GIC redistributor discovery range.
#[derive(Clone, Copy)]
pub struct Gicr {
    pub base: u64,
    pub length: u32,
}

impl Gicr {
    pub const EMPTY: Self = Self {
        base: 0,
        length: 0,
    };
}

/// GIC Interrupt Translation Service.
#[derive(Clone, Copy)]
pub struct Its {
    pub id: u32,
    pub base: u64,
}

impl Its {
    pub const EMPTY: Self = Self { id: 0, base: 0 };
}

/// PCIe ECAM window from MCFG.
#[derive(Clone, Copy)]
pub struct Mcfg {
    pub base: u64,
    pub segment: u16,
    pub start_bus: u8,
    pub end_bus: u8,
}

impl Mcfg {
    pub const EMPTY: Self = Self {
        base: 0,
        segment: 0,
        start_bus: 0,
        end_bus: 0,
    };
}

/// Generic timer description from GTDT.
#[derive(Clone, Copy)]
pub struct Timer {
    pub control_base: u64,
    pub read_base: u64,
    pub secure_el1_gsiv: u32,
    pub secure_el1_flags: u32,
    pub non_secure_el1_gsiv: u32,
    pub non_secure_el1_flags: u32,
    pub virtual_el1_gsiv: u32,
    pub virtual_el1_flags: u32,
    pub non_secure_el2_gsiv: u32,
    pub non_secure_el2_flags: u32,
    pub platform_timers: u32,
}

/// PSCI capabilities advertised by the FADT ARM boot flags.
#[derive(Clone, Copy)]
pub struct Psci {
    pub fadt_present: bool,
    pub compliant: bool,
    pub use_hvc: bool,
}

impl Psci {
    pub const UNKNOWN: Self = Self {
        fadt_present: false,
        compliant: false,
        use_hvc: false,
    };
}

/// One SRAT memory affinity range.
#[derive(Clone, Copy)]
pub struct MemoryDomain {
    pub domain: u32,
    pub base: u64,
    pub length: u64,
    pub enabled: bool,
    pub hot_pluggable: bool,
    pub non_volatile: bool,
}

impl MemoryDomain {
    pub const EMPTY: Self = Self {
        domain: 0,
        base: 0,
        length: 0,
        enabled: false,
        hot_pluggable: false,
        non_volatile: false,
    };
}

/// Boot graphics image description.
///
/// BGRT only carries the image address and its screen offset. The image's own
/// header holds the pixel dimensions, so none are invented here.
#[derive(Clone, Copy)]
pub struct Bgrt {
    pub version: u16,
    pub status: u8,
    pub image_type: u8,
    pub address: u64,
    pub offset_x: u32,
    pub offset_y: u32,
}

/// One debug device from DBG2.
#[derive(Clone, Copy)]
pub struct Dbg2Port {
    /// ACPI namespace path, NUL padded to 16 bytes.
    pub namespace: [u8; 16],
    /// Number of valid bytes in `namespace`.
    pub namespace_length: u8,
    /// Port type, for example `0x8000` (serial).
    pub port_type: u16,
    /// Port subtype, for example `0x13` (ARM S3C/PL011-style UART).
    pub subtype: u16,
    /// Base address from the descriptor's Generic Address Structure.
    pub base: u64,
}

impl Dbg2Port {
    pub const EMPTY: Self = Self {
        namespace: [0; 16],
        namespace_length: 0,
        port_type: 0,
        subtype: 0,
        base: 0,
    };
}

/// TPM 2.0 description.
#[derive(Clone, Copy)]
pub struct Tpm2 {
    pub platform_class: u16,
    pub control_area: u64,
    pub start_method: u32,
}

/// SPCR console description.
#[derive(Clone, Copy)]
pub struct SpcrInfo {
    pub interface: u8,
    pub base: u64,
    pub interrupt_type: u8,
    pub gsiv: u32,
    pub baud: u8,
    pub parity: u8,
    pub stop_bits: u8,
    pub flow_control: u8,
}

/// Everything milestone 0.0.4 understands about the platform.
#[derive(Clone, Copy)]
pub struct Platform {
    pub revision: u8,
    pub oem_id: [u8; 6],
    pub table_count: usize,
    pub tables_ok: usize,
    pub tables_bad: usize,

    pub cpus: [Cpu; MAX_CPUS],
    pub cpu_count: usize,
    pub enabled_cpus: usize,

    pub gicd: Option<Gicd>,
    pub gicr: [Gicr; MAX_GICR],
    pub gicr_count: usize,
    pub its: [Its; MAX_ITS],
    pub its_count: usize,

    pub mcfg: [Mcfg; MAX_MCFG],
    pub mcfg_count: usize,
    pub timer: Option<Timer>,

    pub psci: Psci,
    pub dsdt_address: u64,

    pub has_srat: bool,
    pub has_slit: bool,
    pub memory_domains: [MemoryDomain; MAX_MEMORY_DOMAINS],
    pub memory_domain_count: usize,
    pub locality_count: u32,
    pub locality_distance: [[u8; 8]; 8],

    pub pptt_nodes: usize,
    pub pptt_caches: usize,
    pub pptt_node_info: [PpttNode; MAX_PPTT_NODES],
    pub pptt_node_count: usize,
    pub pptt_clusters: usize,
    /// Cluster index per ACPI processor UID, or [`PPTT_CLUSTER_UNKNOWN`].
    pub pptt_processor_cluster: [u16; MAX_CPUS],
    pub iort_nodes: usize,
    pub bgrt: Option<Bgrt>,
    pub tpm2: Option<Tpm2>,
    pub spcr: Option<SpcrInfo>,
    pub dbg2: [Dbg2Port; MAX_DBG2_PORTS],
    pub dbg2_count: usize,
    pub dbg2_ports: usize,
    pub gic_msi_frames: usize,
}

impl Platform {
    pub const fn empty() -> Self {
        Self {
            revision: 0,
            oem_id: [0; 6],
            table_count: 0,
            tables_ok: 0,
            tables_bad: 0,
            cpus: [Cpu::EMPTY; MAX_CPUS],
            cpu_count: 0,
            enabled_cpus: 0,
            gicd: None,
            gicr: [Gicr::EMPTY; MAX_GICR],
            gicr_count: 0,
            its: [Its::EMPTY; MAX_ITS],
            its_count: 0,
            mcfg: [Mcfg::EMPTY; MAX_MCFG],
            mcfg_count: 0,
            timer: None,
            psci: Psci::UNKNOWN,
            dsdt_address: 0,
            has_srat: false,
            has_slit: false,
            memory_domains: [MemoryDomain::EMPTY; MAX_MEMORY_DOMAINS],
            memory_domain_count: 0,
            locality_count: 0,
            locality_distance: [[0; 8]; 8],
            pptt_nodes: 0,
            pptt_caches: 0,
            pptt_node_info: [PpttNode::EMPTY; MAX_PPTT_NODES],
            pptt_node_count: 0,
            pptt_clusters: 0,
            pptt_processor_cluster: [PPTT_CLUSTER_UNKNOWN; MAX_CPUS],
            iort_nodes: 0,
            bgrt: None,
            tpm2: None,
            spcr: None,
            dbg2: [Dbg2Port::EMPTY; MAX_DBG2_PORTS],
            dbg2_count: 0,
            dbg2_ports: 0,
            gic_msi_frames: 0,
        }
    }

    pub fn current_cpu(&self, mpidr: u64) -> Option<&Cpu> {
        // Compare only the affinity fields. MADT entries omit MPIDR_EL1's
        // uniprocessor bit (bit 31) and implementation-defined bits, so a raw
        // comparison fails even though the core is described.
        let needle = mpidr & MPIDR_AFFINITY_MASK;
        self.cpus[..self.cpu_count]
            .iter()
            .find(|cpu| cpu.mpidr & MPIDR_AFFINITY_MASK == needle)
    }
}

/// A read-only platform description shared by every core after SMP starts.
struct PlatformStorage(UnsafeCell<Platform>);

// SAFETY: `install` is called once on the boot core before any secondary is
// started. After that the storage is never written, so concurrent reads are
// safe.
unsafe impl Sync for PlatformStorage {}

static PLATFORM: PlatformStorage = PlatformStorage(UnsafeCell::new(Platform::empty()));
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Copy the decoded platform into the shared static and return a reference
/// valid for the rest of the boot. Called once, before SMP startup.
pub fn install(platform: &Platform) -> &'static Platform {
    // SAFETY: single-threaded boot-core initialization; the reference is only
    // handed out after the store.
    unsafe {
        ptr::write(PLATFORM.0.get(), *platform);
    }
    INSTALLED.store(true, Ordering::Release);
    // SAFETY: the storage is initialized and never mutated again.
    unsafe { &*PLATFORM.0.get() }
}

pub fn installed() -> Option<&'static Platform> {
    if INSTALLED.load(Ordering::Acquire) {
        // SAFETY: see `install`.
        Some(unsafe { &*PLATFORM.0.get() })
    } else {
        None
    }
}

/// Validate the RSDP, walk the root table, and decode every table this
/// milestone understands. Returns `None` when the firmware description is not
/// usable.
pub fn discover(info: &BootInfo) -> Option<Platform> {
    if !info.has_flag(FLAG_RSDP_VALID) || info.rsdp_address == 0 {
        println!("ACPI: no valid RSDP described by the loader");
        return None;
    }

    // SAFETY: the loader validated this RSDP address.
    let rsdp = unsafe { Rsdp::parse(info.rsdp_address) }?;
    println!(
        "ACPI RSDP: rev {}, OEM \"{}\", root {} @ 0x{:016x}, RSDP @ 0x{:016x}, checksum {}",
        rsdp.revision,
        ascii_padded(&rsdp.oem_id),
        if rsdp.root_is_xsdt { "XSDT" } else { "RSDT" },
        rsdp.root_address,
        rsdp.address,
        if rsdp.checksum_ok { "ok" } else { "BAD" }
    );
    if !rsdp.checksum_ok {
        println!("ACPI: refusing to walk tables with a bad RSDP checksum");
        return None;
    }

    let mut platform = Platform::empty();
    platform.revision = rsdp.revision;
    platform.oem_id = rsdp.oem_id;

    let root_expected = if rsdp.root_is_xsdt { *b"XSDT" } else { *b"RSDT" };
    // SAFETY: the RSDP root pointer is firmware-provided.
    let root = unsafe { Table::from_raw(rsdp.root_address) }?;
    if !root.is(&root_expected) || !root.checksum_ok() {
        println!(
            "ACPI: {} at 0x{:016x} is invalid (signature \"{}\", checksum {})",
            ascii_padded(&root_expected),
            rsdp.root_address,
            root.signature_str(),
            if root.checksum_ok() { "ok" } else { "BAD" }
        );
        return None;
    }

    let entry_size = if rsdp.root_is_xsdt { 8 } else { 4 };
    let entries = (root.length() - 36) / entry_size;
    println!(
        "ACPI {}: 0x{:016x}, {} entries, checksum ok",
        root.signature_str(),
        rsdp.root_address,
        entries
    );
    println!("  #  SIG   LEN    REV  OEM-TABLE  CHECK");
    platform.table_count = entries;

    for index in 0..entries {
        let address = if entry_size == 8 {
            root.u64(36 + index * 8)
        } else {
            root.u32(36 + index * 4) as u64
        };
        if address == 0 {
            continue;
        }

        // SAFETY: every entry is a firmware table pointer in the root table.
        let Some(table) = (unsafe { Table::from_raw(address) }) else {
            println!("  {:>2}  ????  ???    ?    ?          BAD", index);
            platform.tables_bad += 1;
            continue;
        };

        if table.checksum_ok() {
            platform.tables_ok += 1;
        } else {
            platform.tables_bad += 1;
        }

        println!(
            "  {:>2}  {:<4}  {:>5}  {:>3}  {:<9}  {}",
            index,
            table.signature_str(),
            table.length(),
            table.revision(),
            ascii_padded(table.oem_table_id()),
            if table.checksum_ok() { "ok" } else { "BAD" }
        );

        match &table.signature() {
            b"APIC" => madt::parse(&table, &mut platform),
            b"PPTT" => pptt::parse(&table, &mut platform),
            b"GTDT" => gtdt::parse(&table, &mut platform),
            b"MCFG" => mcfg::parse(&table, &mut platform),
            b"IORT" => iort::parse(&table, &mut platform),
            b"SRAT" => srat::parse(&table, &mut platform),
            b"SLIT" => srat::parse_slit(&table, &mut platform),
            b"FACP" => facp::parse(&table, &mut platform),
            b"SPCR" => misc::parse_spcr(&table, &mut platform),
            b"DBG2" => dbg2::parse(&table, &mut platform),
            b"BGRT" => misc::parse_bgrt(&table, &mut platform),
            b"TPM2" => misc::parse_tpm2(&table, &mut platform),
            _ => {}
        }
    }

    if platform.tables_bad != 0 {
        println!(
            "ACPI: {} of {} tables failed checksum validation",
            platform.tables_bad, platform.table_count
        );
    }

    Some(platform)
}
