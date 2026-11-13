//! Firmware and platform discovery helpers used by the loader.
//!
//! Protocols are opened non-exclusively. The loader is a normal application:
//! taking exclusive ownership of the GOP would disconnect the UEFI console
//! driver that the remaining diagnostic output depends on.

use alloc::string::ToString;
use sketchos_bootinfo::{pixel_format, FramebufferInfo};
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams};
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};
use uefi::proto::loaded_image::LoadedImage;

/// Firmware vendor as NUL-padded ASCII, truncated to 63 characters.
pub fn firmware_vendor_ascii() -> [u8; 64] {
    let mut out = [0u8; 64];
    let vendor = uefi::system::firmware_vendor().to_string();

    for (slot, ch) in out.iter_mut().zip(vendor.chars()) {
        *slot = if ch.is_ascii() { ch as u8 } else { b'?' };
    }

    out
}

/// Physical range of the running loader image, from the `LoadedImage` protocol.
pub fn loader_range() -> Option<(u64, u64)> {
    let handle = boot::get_handle_for_protocol::<LoadedImage>().ok()?;
    let params = OpenProtocolParams {
        handle,
        agent: boot::image_handle(),
        controller: None,
    };

    // SAFETY: the protocol is opened non-exclusively and immediately dropped;
    // the loader image cannot be unloaded while it is running.
    let image =
        unsafe { boot::open_protocol::<LoadedImage>(params, OpenProtocolAttributes::GetProtocol) }
            .ok()?;

    let (base, size) = image.info();
    if base.is_null() || size == 0 {
        return None;
    }

    let start = base as u64;
    Some((start, start.checked_add(size)?))
}

/// Current UEFI GOP mode, if the firmware exposes a linear framebuffer.
pub fn framebuffer_info() -> Option<FramebufferInfo> {
    let handle = boot::get_handle_for_protocol::<GraphicsOutput>().ok()?;
    let params = OpenProtocolParams {
        handle,
        agent: boot::image_handle(),
        controller: None,
    };

    // SAFETY: the GOP is only queried, never drawn to, and is opened
    // non-exclusively so the UEFI console keeps working.
    let mut gop =
        unsafe { boot::open_protocol::<GraphicsOutput>(params, OpenProtocolAttributes::GetProtocol) }
            .ok()?;

    let mode = gop.current_mode_info();
    let format = match mode.pixel_format() {
        PixelFormat::Rgb => pixel_format::RGB,
        PixelFormat::Bgr => pixel_format::BGR,
        PixelFormat::Bitmask => pixel_format::BITMASK,
        PixelFormat::BltOnly => return None,
    };

    let (width, height) = mode.resolution();
    let stride = mode.stride();
    if width == 0 || height == 0 || stride < width {
        return None;
    }

    // `FrameBuffer` borrows the GOP; extract the raw description and release
    // both before returning to the caller.
    let (address, size) = {
        let mut framebuffer = gop.frame_buffer();
        (framebuffer.as_mut_ptr() as u64, framebuffer.size() as u64)
    };

    if address == 0 || size == 0 {
        return None;
    }

    // Sanity check against the mode description: a stride smaller than the
    // width or a buffer too small for the advertised mode is unusable.
    let minimum = (stride as u64)
        .checked_mul(height as u64)?
        .checked_mul(4)?;
    if size < minimum {
        return None;
    }

    Some(FramebufferInfo {
        address,
        width: width as u32,
        height: height as u32,
        stride: stride as u32,
        pixel_format: format,
        size,
    })
}
