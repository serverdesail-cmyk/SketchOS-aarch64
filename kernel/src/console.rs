//! Early PL011 console.
//!
//! Milestone 0.0.4 only drives an ARM PL011, and only when the firmware
//! describes one. The QEMU `virt` PL011 runs from a 24 MHz reference clock,
//! so that is the divisor used here. A real machine whose firmware describes
//! a non-PL011 UART intentionally gets no kernel output yet: driving an
//! unknown register layout would be worse than staying silent.
//!
//! The base stored here is physical; every register access goes through the
//! linear physical map built by milestone 0.0.4.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU64, Ordering};

use sketchos_bootinfo::{phys_to_virt, FramebufferInfo};

use crate::sync::IrqSpinLock;

// PL011 register offsets.
const UART_DR: usize = 0x00;
const UART_FR: usize = 0x18;
const UART_IBRD: usize = 0x24;
const UART_FBRD: usize = 0x28;
const UART_LCR_H: usize = 0x2c;
const UART_CR: usize = 0x30;
const UART_IMSC: usize = 0x38;
const UART_ICR: usize = 0x44;

const FR_BUSY: u32 = 1 << 3;
const FR_RXFE: u32 = 1 << 4; // RX FIFO empty
const FR_TXFF: u32 = 1 << 5;

const LCR_H_FEN: u32 = 1 << 4;
const LCR_H_WLEN_8: u32 = 3 << 5;

const CR_UARTEN: u32 = 1 << 0;
const CR_TXE: u32 = 1 << 8;
const CR_RXE: u32 = 1 << 9;

/// QEMU `virt` PL011 reference clock.
const PL011_CLOCK_HZ: u64 = 24_000_000;
const BAUD_115200: u64 = 115_200;

/// Physical base of the active UART, or zero when no console is available.
static UART_BASE: AtomicU64 = AtomicU64::new(0);

/// Serializes complete formatted writes across cores. IRQs are masked while
/// the lock is held, so an interrupt handler cannot deadlock against the core
/// that was printing.
static CONSOLE_LOCK: IrqSpinLock<()> = IrqSpinLock::new(());

pub fn init(base: u64) {
    if base == 0 || base & 0x3 != 0 {
        return;
    }

    UART_BASE.store(base, Ordering::Relaxed);

    let divisor = PL011_CLOCK_HZ / (16 * BAUD_115200);
    let remainder = PL011_CLOCK_HZ % (16 * BAUD_115200);
    let fractional = (remainder * 64 + (16 * BAUD_115200) / 2) / (16 * BAUD_115200);

    // SAFETY: `base` came from a firmware description of a PL011.
    unsafe {
        mmio_write(base, UART_CR, 0);
        let mut spins = 0u32;
        while mmio_read(base, UART_FR) & FR_BUSY != 0 && spins < 1_000_000 {
            spins += 1;
        }
        mmio_write(base, UART_ICR, 0x7ff);
        mmio_write(base, UART_IBRD, divisor as u32);
        mmio_write(base, UART_FBRD, fractional as u32);
        mmio_write(base, UART_LCR_H, LCR_H_FEN | LCR_H_WLEN_8);
        // No interrupts: polling output only.
        mmio_write(base, UART_IMSC, 0);
        mmio_write(base, UART_CR, CR_UARTEN | CR_TXE | CR_RXE);
    }
}

/// Activate the GOP framebuffer text console. The UART, if present, remains
/// active; output is mirrored to both consoles.
pub fn init_framebuffer(info: &FramebufferInfo) -> Result<(), &'static str> {
    crate::fb::init(info)
}

/// True when the framebuffer console is drawing output.
pub fn framebuffer_active() -> bool {
    crate::fb::is_active()
}

pub fn write_byte(byte: u8) {
    // The framebuffer is independent of the UART: on the laptop the firmware
    // describes a Qualcomm UART that this milestone cannot drive, so the screen
    // may be the only visible console.
    crate::fb::write_byte(byte);

    let base = UART_BASE.load(Ordering::Relaxed);
    if base == 0 {
        return;
    }

    // SAFETY: `base` was validated by `init` and points at a PL011.
    unsafe {
        let mut spins = 0u32;
        while mmio_read(base, UART_FR) & FR_TXFF != 0 {
            if spins >= 1_000_000 {
                return;
            }
            spins += 1;
        }
        mmio_write(base, UART_DR, byte as u32);
    }
}

/// True when a supported PL011 receive FIFO is available for shell input.
/// This does not imply USB HID or laptop keyboard support.
pub fn input_available() -> bool {
    UART_BASE.load(Ordering::Relaxed) != 0
}

/// Read one byte from the PL011 RX FIFO, or None if empty. Non-blocking.
pub fn read_byte() -> Option<u8> {
    let base = UART_BASE.load(Ordering::Relaxed);
    if base == 0 {
        return None;
    }
    // SAFETY: base validated by init.
    unsafe {
        if mmio_read(base, UART_FR) & FR_RXFE != 0 {
            None
        } else {
            Some((mmio_read(base, UART_DR) & 0xff) as u8)
        }
    }
}

#[allow(dead_code)]
pub fn write_bytes(bytes: &[u8]) {
    let _guard = CONSOLE_LOCK.lock();
    write_bytes_unlocked(bytes);
}

fn write_bytes_unlocked(bytes: &[u8]) {
    for &byte in bytes {
        if byte == b'\n' {
            write_byte(b'\r');
        }
        write_byte(byte);
    }
}

/// Release the console lock from the panic path. The panicking core may have
/// been interrupted while printing, and a stuck lock would hide the panic.
pub fn force_unlock() {
    CONSOLE_LOCK.force_unlock();
}

struct Console;

impl Write for Console {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        write_bytes_unlocked(text.as_bytes());
        Ok(())
    }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments<'_>) {
    // Hold the lock for the whole formatted message, not one fragment at a
    // time, so lines from different cores never interleave.
    let _guard = CONSOLE_LOCK.lock();
    let _ = Console.write_fmt(args);
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::console::_print(core::format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! println {
    () => {
        $crate::print!("\n")
    };
    ($($arg:tt)*) => {
        $crate::print!("{}\n", core::format_args!($($arg)*))
    };
}

unsafe fn mmio_read(base: u64, offset: usize) -> u32 {
    let address = phys_to_virt(base) as usize + offset;
    let address = address as *const u32;
    unsafe { core::ptr::read_volatile(address) }
}

unsafe fn mmio_write(base: u64, offset: usize, value: u32) {
    let address = phys_to_virt(base) as usize + offset;
    let address = address as *mut u32;
    unsafe { core::ptr::write_volatile(address, value) };
}
