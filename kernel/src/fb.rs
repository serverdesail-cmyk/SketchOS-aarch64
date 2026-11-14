//! UEFI GOP framebuffer text console.
//!
//! The loader passes the current GOP linear framebuffer in the boot record.
//! This module draws an 8x8 bitmap font at an integer scale so the kernel has
//! visible output on a real laptop even when the firmware describes a
//! non-PL011 debug UART that the kernel cannot drive yet.
//!
//! Only 32-bit-per-pixel RGB/BGR modes are supported exactly. Bitmask modes
//! are drawn with a full-intensity white because the boot ABI does not yet
//! carry the channel masks; the geometry is still validated. `BltOnly` modes
//! never reach the kernel because the loader reports no framebuffer for them.
//!
//! All framebuffer writes are volatile and bounds-checked against the size the
//! firmware reported. The module does not create the mapping: it stores the
//! linear-map address of the framebuffer that milestone 0.0.4 mapped Normal
//! non-cacheable.

use core::ptr;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use sketchos_bootinfo::{phys_to_virt, pixel_format, FramebufferInfo};

use crate::font8x8;

const GLYPH_WIDTH: u32 = 8;
const GLYPH_HEIGHT: u32 = 8;
const SCALE: u32 = 2;
const BACKGROUND: u32 = 0x0000_0000;
const FOREGROUND_RGB: u32 = 0x00FF_FFFF;
const FOREGROUND_MASK: u32 = 0xFFFF_FFFF;

static BASE: AtomicU64 = AtomicU64::new(0);
static SIZE: AtomicU64 = AtomicU64::new(0);
static WIDTH: AtomicU32 = AtomicU32::new(0);
static HEIGHT: AtomicU32 = AtomicU32::new(0);
static STRIDE: AtomicU32 = AtomicU32::new(0);
static FORMAT: AtomicU32 = AtomicU32::new(pixel_format::UNKNOWN);
static COLUMNS: AtomicU32 = AtomicU32::new(0);
static ROWS: AtomicU32 = AtomicU32::new(0);
static CURSOR_X: AtomicU32 = AtomicU32::new(0);
static CURSOR_Y: AtomicU32 = AtomicU32::new(0);

/// Try to activate the framebuffer console.
///
/// Returns a diagnostic reason when the description is unusable. The caller
/// keeps the UART console (if any) either way.
pub fn init(info: &FramebufferInfo) -> Result<(), &'static str> {
    if info.address == 0 {
        return Err("framebuffer address is zero");
    }
    if info.width == 0 || info.height == 0 {
        return Err("framebuffer dimensions are zero");
    }
    if info.stride < info.width {
        return Err("framebuffer stride is smaller than its width");
    }
    if info.pixel_format != pixel_format::RGB
        && info.pixel_format != pixel_format::BGR
        && info.pixel_format != pixel_format::BITMASK
    {
        return Err("unsupported framebuffer pixel format");
    }

    let required = (info.stride as u64)
        .checked_mul(info.height as u64)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("framebuffer size overflow")?;
    if info.size < required {
        return Err("framebuffer is smaller than its advertised geometry");
    }

    let glyph_width = GLYPH_WIDTH * SCALE;
    let glyph_height = GLYPH_HEIGHT * SCALE;
    let columns = info.width / glyph_width;
    let rows = info.height / glyph_height;
    if columns == 0 || rows == 0 {
        return Err("framebuffer is too small for the text console");
    }

    // Keep the global framebuffer disabled until the first scanout clear
    // succeeds. If the firmware's GOP mapping is stale, the exception path
    // must not recursively write to the same invalid framebuffer.
    BASE.store(0, Ordering::Release);
    let base = phys_to_virt(info.address);
    clear_region(base, info.size, info.stride, info.height);

    SIZE.store(info.size, Ordering::Relaxed);
    WIDTH.store(info.width, Ordering::Relaxed);
    HEIGHT.store(info.height, Ordering::Relaxed);
    STRIDE.store(info.stride, Ordering::Relaxed);
    FORMAT.store(info.pixel_format, Ordering::Relaxed);
    COLUMNS.store(columns, Ordering::Relaxed);
    ROWS.store(rows, Ordering::Relaxed);
    CURSOR_X.store(0, Ordering::Relaxed);
    CURSOR_Y.store(0, Ordering::Relaxed);
    BASE.store(base, Ordering::Release);
    Ok(())
}

/// True when [`init`] succeeded.
pub fn is_active() -> bool {
    BASE.load(Ordering::Acquire) != 0
}

/// Write one byte to the framebuffer. Ignored when the console is inactive.
pub fn write_byte(byte: u8) {
    if !is_active() {
        return;
    }

    match byte {
        b'\n' => newline(),
        b'\r' => CURSOR_X.store(0, Ordering::Relaxed),
        b'\t' => {
            let columns = COLUMNS.load(Ordering::Relaxed);
            let mut x = CURSOR_X.load(Ordering::Relaxed);
            x = (x + 4) & !3;
            if x >= columns {
                newline();
            } else {
                CURSOR_X.store(x, Ordering::Relaxed);
            }
        }
        0x08 => backspace(),
        0x20..=0x7e => draw_character(byte),
        _ => {}
    }
}

fn newline() {
    let rows = ROWS.load(Ordering::Relaxed);
    let y = CURSOR_Y.load(Ordering::Relaxed).saturating_add(1);
    CURSOR_X.store(0, Ordering::Relaxed);

    if y >= rows {
        scroll();
        CURSOR_Y.store(rows.saturating_sub(1), Ordering::Relaxed);
    } else {
        CURSOR_Y.store(y, Ordering::Relaxed);
    }
}

fn backspace() {
    let columns = COLUMNS.load(Ordering::Relaxed);
    let mut x = CURSOR_X.load(Ordering::Relaxed);
    let mut y = CURSOR_Y.load(Ordering::Relaxed);

    if x != 0 {
        x -= 1;
    } else if y != 0 {
        y -= 1;
        x = columns.saturating_sub(1);
    } else {
        return;
    }

    CURSOR_X.store(x, Ordering::Relaxed);
    CURSOR_Y.store(y, Ordering::Relaxed);
    clear_cell(x, y);
}

fn draw_character(byte: u8) {
    let columns = COLUMNS.load(Ordering::Relaxed);
    let x = CURSOR_X.load(Ordering::Relaxed);
    let y = CURSOR_Y.load(Ordering::Relaxed);

    draw_glyph(byte, x, y);

    let next = x.saturating_add(1);
    if next >= columns {
        newline();
    } else {
        CURSOR_X.store(next, Ordering::Relaxed);
    }
}

fn draw_glyph(byte: u8, cell_x: u32, cell_y: u32) {
    let glyph = font8x8::BASIC[(byte as usize) & 0x7f];
    let origin_x = cell_x * GLYPH_WIDTH * SCALE;
    let origin_y = cell_y * GLYPH_HEIGHT * SCALE;
    let foreground = foreground_color();

    for row in 0..GLYPH_HEIGHT {
        let bits = glyph[row as usize];
        for column in 0..GLYPH_WIDTH {
            let color = if bits & (1 << column) != 0 {
                foreground
            } else {
                BACKGROUND
            };
            fill_rect(
                origin_x + column * SCALE,
                origin_y + row * SCALE,
                SCALE,
                SCALE,
                color,
            );
        }
    }
}

fn clear_cell(cell_x: u32, cell_y: u32) {
    fill_rect(
        cell_x * GLYPH_WIDTH * SCALE,
        cell_y * GLYPH_HEIGHT * SCALE,
        GLYPH_WIDTH * SCALE,
        GLYPH_HEIGHT * SCALE,
        BACKGROUND,
    );
}

fn clear_region(base: u64, size: u64, stride: u32, height: u32) {
    let row_bytes = stride as u64 * 4;
    if base == 0 || stride == 0 {
        return;
    }

    for row in 0..height as u64 {
        let offset = row * row_bytes;
        if offset.saturating_add(row_bytes) > size {
            break;
        }
        // SAFETY: every 32-bit pixel lies inside the validated firmware range.
        // Volatile stores prevent the compiler from eliding scanout writes.
        unsafe {
            let pixels = row_bytes / 4;
            for column in 0..pixels {
                ptr::write_volatile((base + offset + column * 4) as *mut u32, 0);
            }
        }
    }
    // Ensure the display engine can observe the cleared scanout buffer.
    unsafe { core::arch::asm!("dsb oshst", options(nostack, preserves_flags)) };
}

fn scroll() {
    let base = BASE.load(Ordering::Relaxed);
    let stride = STRIDE.load(Ordering::Relaxed);
    let rows = ROWS.load(Ordering::Relaxed);
    let size = SIZE.load(Ordering::Relaxed);
    let glyph_height = GLYPH_HEIGHT * SCALE;
    let row_bytes = stride as u64 * glyph_height as u64 * 4;

    if base == 0 || rows < 2 || row_bytes == 0 {
        return;
    }

    let total = row_bytes.saturating_mul(rows as u64);
    if total > size {
        return;
    }

    // Move every text row up by one using volatile 32-bit accesses. The
    // destination precedes the source, so a forward copy is overlap-safe.
    // SAFETY: total was checked against SIZE above.
    unsafe {
        let pixels = row_bytes.saturating_mul(rows.saturating_sub(1) as u64) / 4;
        let source = (base + row_bytes) as *const u32;
        let destination = base as *mut u32;
        for index in 0..pixels as usize {
            let value = ptr::read_volatile(source.add(index));
            ptr::write_volatile(destination.add(index), value);
        }

        let last_row = (base + row_bytes * rows.saturating_sub(1) as u64) as *mut u32;
        for index in 0..(row_bytes / 4) as usize {
            ptr::write_volatile(last_row.add(index), 0);
        }
    }
    unsafe { core::arch::asm!("dsb oshst", options(nostack, preserves_flags)) };
}

fn fill_rect(x: u32, y: u32, width: u32, height: u32, color: u32) {
    for dy in 0..height {
        for dx in 0..width {
            put_pixel(x.saturating_add(dx), y.saturating_add(dy), color);
        }
    }
}

fn put_pixel(x: u32, y: u32, color: u32) {
    let base = BASE.load(Ordering::Relaxed);
    let width = WIDTH.load(Ordering::Relaxed);
    let height = HEIGHT.load(Ordering::Relaxed);
    let stride = STRIDE.load(Ordering::Relaxed);
    let size = SIZE.load(Ordering::Relaxed);

    if base == 0 || x >= width || y >= height {
        return;
    }

    let offset = (y as u64)
        .saturating_mul(stride as u64)
        .saturating_add(x as u64)
        .saturating_mul(4);
    if offset.saturating_add(4) > size {
        return;
    }

    // SAFETY: the pixel lies inside the visible area and the validated
    // framebuffer size. Volatile writes are required for device memory.
    unsafe {
        ptr::write_volatile((base + offset) as *mut u32, color);
    }
}

fn foreground_color() -> u32 {
    match FORMAT.load(Ordering::Relaxed) {
        pixel_format::BITMASK => FOREGROUND_MASK,
        _ => FOREGROUND_RGB,
    }
}
