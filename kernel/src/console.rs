//! Early polling consoles for firmware-described PL011 and Qualcomm GENI UARTs.
//!
//! PL011 is used only when SPCR identifies that exact interface. Qualcomm
//! GENI support is deliberately output-only and FIFO/polling-only: it verifies
//! that firmware loaded the UART protocol and that DMA mode is disabled, then
//! reuses the firmware's clock/baud configuration. It does not reset the
//! serial engine, change clocks, enable DMA, or request interrupts.
//!
//! The base stored here is physical; every register access goes through the
//! linear physical map built by the UEFI loader.

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
static GENI_BASE: AtomicU64 = AtomicU64::new(0);

// Qualcomm GENI Serial Engine register offsets (QUPv3).
const GENI_FW_REVISION_RO: usize = 0x068;
const GENI_DMA_MODE_EN: usize = 0x258;
const GENI_STATUS: usize = 0x040;
const GENI_M_CMD0: usize = 0x600;
const GENI_M_CMD_CTRL: usize = 0x604;
const GENI_M_IRQ_STATUS: usize = 0x610;
const GENI_M_IRQ_EN: usize = 0x614;
const GENI_M_IRQ_CLEAR: usize = 0x618;
const GENI_TX_TRANS_LEN: usize = 0x270;
const GENI_TX_FIFO: usize = 0x700;
const GENI_TX_WATERMARK: usize = 0x80c;

const GENI_PROTOCOL_SHIFT: u32 = 8;
const GENI_PROTOCOL_MASK: u32 = 0xff << GENI_PROTOCOL_SHIFT;
const GENI_PROTOCOL_UART: u32 = 2;
const GENI_DMA_MODE: u32 = 1 << 0;
const GENI_CMD_ACTIVE: u32 = 1 << 0;
const GENI_CMD_DONE: u32 = 1 << 0;
const GENI_CMD_ABORT: u32 = 1 << 1;
const GENI_CMD_ABORTED: u32 = 1 << 5;
const GENI_TX_WATERMARK_REACHED: u32 = 1 << 30;
const GENI_UART_START_TX: u32 = 1;
const GENI_OPCODE_SHIFT: u32 = 27;
const GENI_TX_WATERMARK_LEVEL: u32 = 2;
const GENI_POLL_LIMIT: u32 = 1_000_000;

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

/// Enable a firmware-configured Qualcomm GENI UART for bounded polling output.
///
/// SPCR interface type 0x13 identifies the SDM845 7.372 MHz UART interface.
/// The firmware is expected to have configured its clocks, pin mux and UART
/// protocol. We verify the protocol and refuse to touch a DMA-mode engine;
/// baud/clock programming and sequencer reset are intentionally omitted.
pub fn init_geni(base: u64) -> Result<(), &'static str> {
    if base == 0 || base & 0x3 != 0 {
        return Err("GENI UART base is zero or unaligned");
    }

    // SAFETY: the physical base comes from the validated firmware SPCR table
    // and the loader maps the described UART window as Device memory.
    let revision = unsafe { mmio_read(base, GENI_FW_REVISION_RO) };
    let protocol = (revision & GENI_PROTOCOL_MASK) >> GENI_PROTOCOL_SHIFT;
    if protocol != GENI_PROTOCOL_UART {
        return Err("GENI firmware is not configured for UART");
    }

    // Never write the FIFO if firmware left the engine in DMA mode.
    let mode = unsafe { mmio_read(base, GENI_DMA_MODE_EN) };
    if mode & GENI_DMA_MODE != 0 {
        return Err("GENI UART is in DMA mode; polling FIFO output is unsafe");
    }

    GENI_BASE.store(base, Ordering::Release);
    Ok(())
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
    // Mirror output to the framebuffer, PL011 and (when validated) GENI UART.
    crate::fb::write_byte(byte);

    let base = UART_BASE.load(Ordering::Relaxed);
    if base != 0 {
        // SAFETY: `base` was validated by `init` and points at a PL011.
        unsafe {
            let mut ready = false;
            for _ in 0..1_000_000 {
                if mmio_read(base, UART_FR) & FR_TXFF == 0 {
                    ready = true;
                    break;
                }
                core::hint::spin_loop();
            }
            if ready {
                mmio_write(base, UART_DR, byte as u32);
            }
        }
    }

    let geni = GENI_BASE.load(Ordering::Acquire);
    if geni != 0 && !geni_putc(geni, byte) {
        // A stuck/disabled engine must not stall the rest of the kernel log.
        GENI_BASE.store(0, Ordering::Release);
    }
}

fn geni_wait(base: u64, offset: usize, mask: u32, set: bool) -> bool {
    for _ in 0..GENI_POLL_LIMIT {
        let value = unsafe { mmio_read(base, offset) } & mask;
        if (value != 0) == set {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// Send one byte using the GENI UART FIFO sequencer. All waits are bounded.
fn geni_putc(base: u64, byte: u8) -> bool {
    if !geni_wait(base, GENI_STATUS, GENI_CMD_ACTIVE, false) {
        return false;
    }

    unsafe {
        // Enable only the two status sources consumed by this polling path.
        let irq_enable = mmio_read(base, GENI_M_IRQ_EN)
            | GENI_CMD_DONE
            | GENI_TX_WATERMARK_REACHED;
        mmio_write(base, GENI_M_IRQ_EN, irq_enable);
        mmio_write(
            base,
            GENI_M_IRQ_CLEAR,
            GENI_CMD_DONE | GENI_TX_WATERMARK_REACHED | GENI_CMD_ABORTED,
        );
        mmio_write(base, GENI_TX_WATERMARK, GENI_TX_WATERMARK_LEVEL);
        mmio_write(base, GENI_TX_TRANS_LEN, 1);
        mmio_write(
            base,
            GENI_M_CMD0,
            GENI_UART_START_TX << GENI_OPCODE_SHIFT,
        );
    }

    if !geni_wait(base, GENI_M_IRQ_STATUS, GENI_TX_WATERMARK_REACHED, true) {
        geni_abort(base);
        return false;
    }

    unsafe {
        mmio_write(base, GENI_TX_FIFO, byte as u32);
        mmio_write(base, GENI_M_IRQ_CLEAR, GENI_TX_WATERMARK_REACHED);
    }

    if !geni_wait(base, GENI_M_IRQ_STATUS, GENI_CMD_DONE, true) {
        geni_abort(base);
        return false;
    }

    unsafe { mmio_write(base, GENI_M_IRQ_CLEAR, GENI_CMD_DONE) };
    true
}

fn geni_abort(base: u64) {
    unsafe {
        mmio_write(base, GENI_M_CMD_CTRL, GENI_CMD_ABORT);
    }
    let _ = geni_wait(base, GENI_M_IRQ_STATUS, GENI_CMD_ABORTED, true);
    unsafe {
        mmio_write(
            base,
            GENI_M_IRQ_CLEAR,
            GENI_CMD_DONE | GENI_TX_WATERMARK_REACHED | GENI_CMD_ABORTED,
        );
    }
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
