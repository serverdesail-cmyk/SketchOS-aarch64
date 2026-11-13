//! Runtime core-to-cluster map used by the scheduler's stealing policy.
//!
//! The map is derived from PPTT when firmware describes a cluster hierarchy.
//! If PPTT is absent or does not cover every online core, the kernel falls back
//! to MPIDR affinity fields and finally to a single cluster. The report always
//! names the source so a topology claim is never invented.

use core::sync::atomic::{AtomicU8, Ordering};

use crate::acpi::{self, Platform, PPTT_CLUSTER_UNKNOWN};
use crate::percpu::{self, MAX_SMP_CORES};
use crate::{print, println, smp};

const SOURCE_NONE: u8 = 0;
const SOURCE_PPTT: u8 = 1;
const SOURCE_MPIDR: u8 = 2;
const SOURCE_SINGLE: u8 = 3;

static CORE_CLUSTER: [AtomicU8; MAX_SMP_CORES] = [const { AtomicU8::new(0) }; MAX_SMP_CORES];
static CLUSTER_COUNT: AtomicU8 = AtomicU8::new(1);
static CORE_COUNT: AtomicU8 = AtomicU8::new(1);
static SOURCE: AtomicU8 = AtomicU8::new(SOURCE_NONE);

/// Build the map after [`smp::discover`] has assigned logical core indices.
pub fn init(platform: &Platform) {
    let cores = smp::cpu_count().min(MAX_SMP_CORES);
    CORE_COUNT.store(cores.max(1) as u8, Ordering::Release);

    let mut pptt_cluster = [0u8; MAX_SMP_CORES];
    let mut pptt_ok = cores != 0 && platform.pptt_clusters != 0;
    for core in 0..cores {
        let Some(record) = percpu::record(core) else {
            pptt_ok = false;
            break;
        };
        let mpidr = record.mpidr.load(Ordering::Relaxed);
        let Some(cpu) = platform.current_cpu(mpidr) else {
            pptt_ok = false;
            break;
        };
        let uid = cpu.uid as usize;
        if uid >= platform.pptt_processor_cluster.len() {
            pptt_ok = false;
            break;
        }
        let cluster = platform.pptt_processor_cluster[uid];
        if cluster == PPTT_CLUSTER_UNKNOWN {
            pptt_ok = false;
            break;
        }
        pptt_cluster[core] = cluster as u8;
    }

    if pptt_ok {
        let mut count = 1u8;
        for core in 0..cores {
            CORE_CLUSTER[core].store(pptt_cluster[core], Ordering::Relaxed);
            count = count.max(pptt_cluster[core].saturating_add(1));
        }
        CLUSTER_COUNT.store(count, Ordering::Release);
        SOURCE.store(SOURCE_PPTT, Ordering::Release);
        return;
    }

    // Fallback: group by the MPIDR affinity fields firmware does expose.
    let mut keys = [0u32; MAX_SMP_CORES];
    let mut key_count = 0usize;
    let mut dense = [0u8; MAX_SMP_CORES];
    let mut nonzero = false;
    for core in 0..cores {
        let Some(record) = percpu::record(core) else {
            break;
        };
        let mpidr = record.mpidr.load(Ordering::Relaxed);
        let key = ((mpidr >> 8) & 0xff) as u32 | (((mpidr >> 16) & 0xff) as u32) << 8;
        if key != 0 {
            nonzero = true;
        }
        let mut slot = None;
        for (index, existing) in keys[..key_count].iter().enumerate() {
            if *existing == key {
                slot = Some(index);
                break;
            }
        }
        let slot = match slot {
            Some(slot) => slot,
            None => {
                if key_count >= MAX_SMP_CORES {
                    0
                } else {
                    keys[key_count] = key;
                    key_count += 1;
                    key_count - 1
                }
            }
        };
        dense[core] = slot as u8;
    }

    if nonzero && key_count > 1 {
        let mut count = 1u8;
        for core in 0..cores {
            CORE_CLUSTER[core].store(dense[core], Ordering::Relaxed);
            count = count.max(dense[core].saturating_add(1));
        }
        CLUSTER_COUNT.store(count, Ordering::Release);
        SOURCE.store(SOURCE_MPIDR, Ordering::Release);
    } else {
        for core in 0..MAX_SMP_CORES {
            CORE_CLUSTER[core].store(0, Ordering::Relaxed);
        }
        CLUSTER_COUNT.store(1, Ordering::Release);
        SOURCE.store(SOURCE_SINGLE, Ordering::Release);
    }
}

pub fn core_count() -> usize {
    CORE_COUNT.load(Ordering::Acquire) as usize
}

pub fn cluster_count() -> usize {
    CLUSTER_COUNT.load(Ordering::Acquire).max(1) as usize
}

pub fn cluster_of(core: usize) -> usize {
    if core >= MAX_SMP_CORES {
        return 0;
    }
    CORE_CLUSTER[core].load(Ordering::Relaxed) as usize
}

pub fn same_cluster(a: usize, b: usize) -> bool {
    cluster_of(a) == cluster_of(b)
}

/// Bitmask of logical cores in `cluster`.
pub fn cluster_mask(cluster: usize) -> u16 {
    let mut mask = 0u16;
    for core in 0..core_count().min(16) {
        if cluster_of(core) == cluster {
            mask |= 1 << core;
        }
    }
    mask
}

fn source_name() -> &'static str {
    match SOURCE.load(Ordering::Acquire) {
        SOURCE_PPTT => "PPTT",
        SOURCE_MPIDR => "MPIDR affinity",
        SOURCE_SINGLE => "single cluster",
        _ => "not initialized",
    }
}

pub fn report() {
    let cores = core_count();
    let clusters = cluster_count();
    println!(
        "Topology: {} cores in {} cluster{} ({})",
        cores,
        clusters,
        if clusters == 1 { "" } else { "s" },
        source_name()
    );
    if clusters > 1 {
        for cluster in 0..clusters {
            print!("  cluster {}: cores", cluster);
            for core in 0..cores {
                if cluster_of(core) == cluster {
                    print!(" {}", core);
                }
            }
            println!();
        }
    }
}

/// Diagnostic helper: map an ACPI UID to its PPTT cluster, if present.
#[allow(dead_code)]
pub fn pptt_cluster(platform: &Platform, uid: u32) -> u16 {
    acpi::pptt::processor_cluster(platform, uid)
}
