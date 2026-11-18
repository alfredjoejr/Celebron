//! Minimal 16550 UART (`COM1`, I/O port `0x3F8`) serial logger for the Celebron UEFI Bootloader.

use core::arch::asm;
use core::fmt::{self, Write};

const COM1_PORT: u16 = 0x3F8;

#[inline(always)]
unsafe fn outb(port: u16, val: u8) {
    asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags));
}

#[inline(always)]
unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    asm!("in al, dx", out("al") val, in("dx") port, options(nomem, nostack, preserves_flags));
    val
}

pub struct SerialPort;

impl SerialPort {
    pub fn init() {
        unsafe {
            outb(COM1_PORT + 1, 0x00); // Disable interrupts
            outb(COM1_PORT + 3, 0x80); // Enable DLAB (set baud rate divisor)
            outb(COM1_PORT + 0, 0x01); // Divisor 1 (115200 baud)
            outb(COM1_PORT + 1, 0x00);
            outb(COM1_PORT + 3, 0x03); // 8 bits, no parity, one stop bit
            outb(COM1_PORT + 2, 0xC7); // Enable FIFO, clear them, with 14-byte threshold
            outb(COM1_PORT + 4, 0x0B); // IRQs enabled, RTS/DSR set
        }
    }

    pub fn write_byte(byte: u8) {
        unsafe {
            // Wait for transmit holding register to be empty
            while (inb(COM1_PORT + 5) & 0x20) == 0 {
                core::hint::spin_loop();
            }
            outb(COM1_PORT, byte);
        }
    }
}

impl Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                Self::write_byte(b'\r');
            }
            Self::write_byte(b);
        }
        Ok(())
    }
}

pub fn _serial_print(args: fmt::Arguments) {
    let _ = SerialPort.write_fmt(args);
}

#[macro_export]
macro_rules! serial_println {
    () => ($crate::serial::_serial_print(format_args!("\n")));
    ($($arg:tt)*) => ($crate::serial::_serial_print(format_args!("{}\n", format_args!($($arg)*))));
}
