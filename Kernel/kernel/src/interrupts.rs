//! Celebron OS Hardware Interrupt Controller & Local APIC Periodic Timer.
//! Remaps & masks legacy 8259 PICs, auto-detects xAPIC MMIO vs. x2APIC MSR mode,
//! programs the periodic Local APIC timer on Vector 32 (0x20), and tracks live ticks.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const IA32_APIC_BASE_MSR: u32 = 0x1B;
const APIC_BASE_ENABLE_BIT: u64 = 1 << 11;
const APIC_BASE_X2APIC_BIT: u64 = 1 << 10;

// Local APIC Register Offsets (xAPIC MMIO; x2APIC MSR = 0x800 + (offset >> 4))
const APIC_REG_TPR: u32 = 0x080;
const APIC_REG_EOI: u32 = 0x0B0;
const APIC_REG_SVR: u32 = 0x0F0;
const APIC_REG_LVT_TIMER: u32 = 0x320;
const APIC_REG_TIMER_INIT_COUNT: u32 = 0x380;
const APIC_REG_TIMER_DIVIDE: u32 = 0x3E0;

pub const TIMER_INTERRUPT_VECTOR: u32 = 32;
const APIC_SPURIOUS_VECTOR: u32 = 0xFF;
const APIC_TIMER_PERIODIC_MODE: u32 = 1 << 17;
const APIC_TIMER_DIVIDE_BY_16: u32 = 0x03;
const APIC_TIMER_INITIAL_COUNT: u32 = 500_000;

static IS_X2APIC: AtomicBool = AtomicBool::new(false);
static XAPIC_MMIO_BASE: AtomicU64 = AtomicU64::new(0xFEE0_0000);
static TIMER_TICKS: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
pub struct ApicInfo {
    pub is_x2apic: bool,
    pub base_addr: u64,
}

#[inline(always)]
unsafe fn outb(port: u16, val: u8) {
    asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags));
}

#[inline(always)]
unsafe fn io_wait() {
    outb(0x80, 0);
}

#[inline(always)]
unsafe fn rdmsr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high, options(nomem, nostack, preserves_flags));
    ((high as u64) << 32) | (low as u64)
}

#[inline(always)]
unsafe fn wrmsr(msr: u32, val: u64) {
    let low = val as u32;
    let high = (val >> 32) as u32;
    asm!("wrmsr", in("ecx") msr, in("eax") low, in("edx") high, options(nomem, nostack, preserves_flags));
}

/// Write a 32-bit Local APIC register using the active x2APIC (MSR) or xAPIC (MMIO) backend.
#[inline(always)]
unsafe fn write_apic_reg(reg_offset: u32, value: u32) {
    if IS_X2APIC.load(Ordering::Relaxed) {
        let msr = 0x800 + (reg_offset >> 4);
        wrmsr(msr, value as u64);
    } else {
        let mmio = (XAPIC_MMIO_BASE.load(Ordering::Relaxed) + reg_offset as u64) as *mut u32;
        mmio.write_volatile(value);
    }
}

/// Remap legacy Master/Slave 8259 PICs to vectors 32..47 and mask all IRQ lines
/// so legacy interrupts cannot collide with CPU exception vectors 0..31.
fn remap_and_mask_pic8259() {
    unsafe {
        // ICW1: start initialization sequence in cascade mode
        outb(0x20, 0x11);
        io_wait();
        outb(0xA0, 0x11);
        io_wait();

        // ICW2: remap vector offsets (Master = 0x20, Slave = 0x28)
        outb(0x21, 0x20);
        io_wait();
        outb(0xA1, 0x28);
        io_wait();

        // ICW3: Master/Slave wiring
        outb(0x21, 0x04);
        io_wait();
        outb(0xA1, 0x02);
        io_wait();

        // ICW4: 8086 mode
        outb(0x21, 0x01);
        io_wait();
        outb(0xA1, 0x01);
        io_wait();

        // Mask all legacy 8259 PIC interrupts (APIC handles interrupts exclusively)
        outb(0x21, 0xFF);
        io_wait();
        outb(0xA1, 0xFF);
        io_wait();
    }
}

/// Mask the legacy 8259 PIC, initialize Local APIC (xAPIC or x2APIC),
/// and configure the periodic APIC timer on Vector 32.
pub fn init_apic_and_timer() -> ApicInfo {
    remap_and_mask_pic8259();

    unsafe {
        let mut apic_base = rdmsr(IA32_APIC_BASE_MSR);
        if (apic_base & APIC_BASE_ENABLE_BIT) == 0 {
            apic_base |= APIC_BASE_ENABLE_BIT;
            wrmsr(IA32_APIC_BASE_MSR, apic_base);
        }

        let is_x2apic = (apic_base & APIC_BASE_X2APIC_BIT) != 0;
        let base_addr = apic_base & 0xFFFF_F000;

        IS_X2APIC.store(is_x2apic, Ordering::Relaxed);
        XAPIC_MMIO_BASE.store(base_addr, Ordering::Relaxed);

        // Clear Task Priority Register so all interrupt priorities are accepted
        write_apic_reg(APIC_REG_TPR, 0);

        // Enable Local APIC via Spurious Interrupt Vector Register (bit 8 = APIC SW Enable)
        write_apic_reg(APIC_REG_SVR, (1 << 8) | APIC_SPURIOUS_VECTOR);

        // Configure periodic timer on Vector 32
        write_apic_reg(APIC_REG_TIMER_DIVIDE, APIC_TIMER_DIVIDE_BY_16);
        write_apic_reg(
            APIC_REG_LVT_TIMER,
            APIC_TIMER_PERIODIC_MODE | TIMER_INTERRUPT_VECTOR,
        );
        write_apic_reg(APIC_REG_TIMER_INIT_COUNT, APIC_TIMER_INITIAL_COUNT);

        ApicInfo {
            is_x2apic,
            base_addr,
        }
    }
}

/// Called from `rust_interrupt_dispatch` on Vector 32 (`0x20`).
#[inline(always)]
pub fn on_timer_tick() {
    TIMER_TICKS.fetch_add(1, Ordering::Relaxed);
    unsafe {
        write_apic_reg(APIC_REG_EOI, 0);
    }
}

#[inline(always)]
pub fn timer_ticks() -> u64 {
    TIMER_TICKS.load(Ordering::Relaxed)
}

/// Enable hardware interrupts (`sti`) and wait for at least 5 live Local APIC timer ticks.
pub fn enable_interrupts_and_verify_timer() -> u64 {
    unsafe {
        asm!("sti", options(nomem, nostack));
    }

    for _ in 0..150_000_000u64 {
        let ticks = timer_ticks();
        if ticks >= 5 {
            return ticks;
        }
        core::hint::spin_loop();
    }

    panic!("Local APIC Timer did not fire after enabling interrupts (sti)");
}
