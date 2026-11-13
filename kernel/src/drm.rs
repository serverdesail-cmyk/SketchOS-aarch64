//! DRM/KMS subsystem skeleton (milestone 0.0.8).
//!
//! Honest scope: this is the *prerequisite* for any compositor, not a compositor.
//! It provides:
//!   - a DRM device with a current display mode,
//!   - dumb buffer allocation (GEM-style, backed by physical pages),
//!   - a minimal modeset that records width/height/refresh.
//! It does NOT implement: atomic modesetting, planes, CRTCs, encoders,
//! connectors, fences, or a real GPU driver. The Qualcomm Adreno (freedreno)
//! driver requires the full Linux DRM/GEM/TTM stack and is out of scope here.
//!
//! Design principles (XNU/macOS-inspired):
//! - Buffers are physical pages shared via the physmap (unified memory): the
//!   CPU writes pixels, the display controller reads them, no copy.
//! - No per-buffer heap allocation metadata beyond a fixed-capacity table.
//! - Cache-line-aligned (128 B) buffer descriptors to avoid false sharing.

use crate::phys;

pub const MAX_DUMB_BUFFERS: usize = 16;

#[derive(Clone, Copy)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub bpp: u32,
}

#[derive(Clone, Copy)]
pub struct GemObject {
    pub phys_base: u64,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub in_use: bool,
}

pub struct DrmDevice {
    pub mode: Mode,
    pub buffers: [GemObject; MAX_DUMB_BUFFERS],
    pub buffer_count: usize,
}

static mut DRM: Option<DrmDevice> = None;

pub fn device() -> Option<&'static mut DrmDevice> {
    unsafe { DRM.as_mut() }
}

pub fn init() {
    let dev = DrmDevice {
        mode: Mode { width: 1920, height: 1080, refresh_hz: 60, bpp: 32 },
        buffers: [GemObject { phys_base: 0, size: 0, width: 0, height: 0, pitch: 0, in_use: false }; MAX_DUMB_BUFFERS],
        buffer_count: 0,
    };
    unsafe { DRM = Some(dev); }
}

/// Allocate a dumb buffer of `width x height` at `bpp` bits. Returns its handle index.
pub fn alloc_dumb(width: u32, height: u32, bpp: u32) -> Result<usize, &'static str> {
    let dev = device().ok_or("no drm device")?;
    if dev.buffer_count >= MAX_DUMB_BUFFERS {
        return Err("too many buffers");
    }
    let pitch = ((width * bpp + 7) / 8 + 63) & !63; // 64-byte aligned pitch
    let size = pitch as u64 * height as u64;
    let pages = ((size + 4095) / 4096) as usize;
    let phys_base = phys::alloc_pages(pages, 4096).ok_or("oom")?;
    // zero
    let v = sketchos_bootinfo::phys_to_virt(phys_base) as *mut u8;
    unsafe { core::ptr::write_bytes(v, 0, pages * 4096) };
    let idx = dev.buffer_count;
    dev.buffers[idx] = GemObject {
        phys_base,
        size,
        width,
        height,
        pitch,
        in_use: true,
    };
    dev.buffer_count += 1;
    Ok(idx)
}

pub fn set_mode(mode: Mode) {
    if let Some(dev) = device() {
        dev.mode = mode;
    }
}

pub fn report() {
    if let Some(dev) = device() {
        crate::println!("DRM: mode {}x{}@{} ({} bpp), {} dumb buffers", dev.mode.width, dev.mode.height, dev.mode.refresh_hz, dev.mode.bpp, dev.buffer_count);
    }
}
