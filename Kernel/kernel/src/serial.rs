//! Ring-0 16550 UART (`COM1`, `0x3F8`) serial port driver for the Celebron Microkernel.

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

pub struct KernelSerial;

impl KernelSerial {
    pub fn write_byte(byte: u8) {
        unsafe {
            while (inb(COM1_PORT + 5) & 0x20) == 0 {
                core::hint::spin_loop();
            }
            outb(COM1_PORT, byte);
        }
    }
}

impl Write for KernelSerial {
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

pub fn _kprint(args: fmt::Arguments) {
    let _ = KernelSerial.write_fmt(args);
}

#[macro_export]
macro_rules! kprintln {
    () => ($crate::serial::_kprint(format_args!("\n")));
    ($($arg:tt)*) => ($crate::serial::_kprint(format_args!("{}\n", format_args!($($arg)*))));
}
