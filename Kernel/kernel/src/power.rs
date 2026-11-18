//! Celebron OS Power Management & Hardware Control Subsystem.
//! Provides clean hardware Shutdown, Cold Reboot, and Warm Reboot via:
//! 1. UEFI 2.x Runtime Services (`ResetSystem`) — native support on Hyper-V Gen 2, OVMF & UEFI PCs
//! 2. ACPI FADT Hardware Registers (`PM1a_CNT_BLK` + `_S5_`, `SLEEP_CONTROL_REG`, `RESET_REG`)
//! 3. Hypervisor / Legacy Fallbacks (QEMU `0x604`, VirtualBox `0x4004`, PCI `0xCF9`, 8042 `0x64`)
//!
//! Also provides safe interactive power-signal polling over COM1 Serial and PS/2 Keyboard,
//! plus atomic signal queueing from capability-gated Ring-3 `SYS_POWER_CONTROL` syscalls.

use crate::acpi::{AcpiInfo, GenericAddressStructure};
use core::arch::asm;
use core::ptr::{addr_of_mut, read_unaligned};
use core::sync::atomic::{AtomicU8, Ordering};

const EFI_RUNTIME_SERVICES_SIGNATURE: u64 = 0x5652_4553_544E_5552; // "RUNTSERV"
const EFI_RESET_SYSTEM_OFFSET: u64 = 0x68;

const EFI_RESET_COLD: u32 = 0;
const EFI_RESET_WARM: u32 = 1;
const EFI_RESET_SHUTDOWN: u32 = 2;

type EfiResetSystemFn = unsafe extern "efiapi" fn(
    reset_type: u32,
    reset_status: usize,
    data_size: usize,
    reset_data: *const u8,
) -> !;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PowerAction {
    None = 0,
    Shutdown = 1,
    RebootCold = 2,
    RebootWarm = 3,
}

impl PowerAction {
    pub fn from_u64(val: u64) -> Option<Self> {
        match val {
            1 => Some(Self::Shutdown),
            2 => Some(Self::RebootCold),
            3 => Some(Self::RebootWarm),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PowerCapabilities {
    pub uefi_reset_available: bool,
    pub acpi_s5_available: bool,
    pub acpi_reset_available: bool,
    pub hw_reduced_acpi: bool,
}

struct PowerState {
    uefi_rt_addr: u64,
    uefi_reset_fn: Option<EfiResetSystemFn>,
    acpi: AcpiInfo,
}

static mut POWER_STATE: PowerState = PowerState {
    uefi_rt_addr: 0,
    uefi_reset_fn: None,
    acpi: AcpiInfo::empty(),
};

static PENDING_SIGNAL: AtomicU8 = AtomicU8::new(0);

#[inline(always)]
unsafe fn outb(port: u16, val: u8) {
    asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags));
}

#[inline(always)]
unsafe fn outw(port: u16, val: u16) {
    asm!("out dx, ax", in("dx") port, in("ax") val, options(nomem, nostack, preserves_flags));
}

#[inline(always)]
unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    asm!("in al, dx", out("al") val, in("dx") port, options(nomem, nostack, preserves_flags));
    val
}

/// Initialize the Power Management subsystem with UEFI Runtime Services & parsed ACPI FADT info.
pub fn init(uefi_runtime_services: u64, acpi_info: AcpiInfo) -> PowerCapabilities {
    let mut uefi_reset_fn: Option<EfiResetSystemFn> = None;

    if uefi_runtime_services != 0 {
        unsafe {
            let sig = read_unaligned(uefi_runtime_services as *const u64);
            if sig == EFI_RUNTIME_SERVICES_SIGNATURE {
                let fn_ptr_addr =
                    read_unaligned((uefi_runtime_services + EFI_RESET_SYSTEM_OFFSET) as *const u64);
                if fn_ptr_addr != 0 {
                    uefi_reset_fn = Some(core::mem::transmute::<u64, EfiResetSystemFn>(fn_ptr_addr));
                }
            }
        }
    }

    unsafe {
        let st = &mut *addr_of_mut!(POWER_STATE);
        st.uefi_rt_addr = uefi_runtime_services;
        st.uefi_reset_fn = uefi_reset_fn;
        st.acpi = acpi_info;
    }

    PowerCapabilities {
        uefi_reset_available: uefi_reset_fn.is_some(),
        acpi_s5_available: acpi_info.pm1a_cnt_blk != 0 || acpi_info.sleep_control_reg.address != 0,
        acpi_reset_available: acpi_info.reset_reg_supported && acpi_info.reset_reg.address != 0,
        hw_reduced_acpi: acpi_info.hw_reduced,
    }
}

/// Queue a power signal (callable safely from Ring-0 or Ring-3 `SYS_POWER_CONTROL` syscall).
pub fn signal_power_action(action: PowerAction) {
    if action != PowerAction::None {
        PENDING_SIGNAL.store(action as u8, Ordering::Release);
    }
}

/// Check for pending power signals from syscalls, COM1 Serial RX, or PS/2 Keyboard.
pub fn poll_power_signal() -> PowerAction {
    // 1. Check queued syscall/kernel power signal
    let pending = PENDING_SIGNAL.swap(0, Ordering::AcqRel);
    match pending {
        1 => return PowerAction::Shutdown,
        2 => return PowerAction::RebootCold,
        3 => return PowerAction::RebootWarm,
        _ => {}
    }

    unsafe {
        // 2. Poll COM1 Serial RX (`0x3FD` Line Status Register bit 0 = Data Ready)
        let lsr = inb(0x3FD);
        if lsr != 0xFF && (lsr & 0x01) != 0 {
            let ch = inb(0x3F8);
            match ch {
                b's' | b'S' => return PowerAction::Shutdown,
                b'r' | b'R' => return PowerAction::RebootCold,
                b'w' | b'W' => return PowerAction::RebootWarm,
                _ => {}
            }
        }

        // 3. Poll Legacy PS/2 8042 Keyboard Controller (`0x64` status)
        // Guarded against 0xFF floating bus on legacy-free Hyper-V Gen 2 VMs.
        let kbd_status = inb(0x64);
        if kbd_status != 0xFF && (kbd_status & 0x01) != 0 && (kbd_status & 0x20) == 0 {
            let scancode = inb(0x60);
            match scancode {
                0x1F => return PowerAction::Shutdown,   // 'S' key (Set 1 make code)
                0x13 => return PowerAction::RebootCold, // 'R' key
                0x11 => return PowerAction::RebootWarm, // 'W' key
                _ => {}
            }
        }
    }

    PowerAction::None
}

unsafe fn write_gas_u8(gas: GenericAddressStructure, val: u8) {
    if gas.address == 0 {
        return;
    }
    match gas.address_space_id {
        0 => {
            // System Memory (MMIO)
            core::ptr::write_volatile(gas.address as *mut u8, val);
        }
        1 => {
            // System I/O Port
            outb(gas.address as u16, val);
        }
        _ => {}
    }
}

/// Execute clean hardware Shutdown (never returns).
pub fn execute_shutdown() -> ! {
    crate::kprintln!("[CELEBRON-POWER] Executing clean system SHUTDOWN...");
    unsafe {
        asm!("cli", options(nomem, nostack, preserves_flags));
        let st = &*addr_of_mut!(POWER_STATE);

        // Tier 1: ACPI HW-Reduced Sleep Control Register (Hyper-V Gen 2 / Modern UEFI)
        if st.acpi.hw_reduced && st.acpi.sleep_control_reg.address != 0 {
            let slp_val = (((st.acpi.slp_typa & 0x07) as u8) << 2) | (1 << 5); // SLP_EN = bit 5
            crate::kprintln!(
                "[CELEBRON-POWER] Writing ACPI HW-Reduced SLEEP_CONTROL_REG (slp_typa={})...",
                st.acpi.slp_typa
            );
            write_gas_u8(st.acpi.sleep_control_reg, slp_val);
            for _ in 0..2_000_000u64 {
                core::hint::spin_loop();
            }
        }

        // Tier 2: Standard ACPI FADT PM1a_CNT_BLK / PM1b_CNT_BLK S5 transition
        if st.acpi.pm1a_cnt_blk != 0 {
            let cmd = ((st.acpi.slp_typa & 0x07) << 10) | (1 << 13); // SLP_EN = bit 13
            crate::kprintln!(
                "[CELEBRON-POWER] Writing ACPI PM1a_CNT_BLK 0x{:X} <- 0x{:04X}...",
                st.acpi.pm1a_cnt_blk,
                cmd
            );
            outw(st.acpi.pm1a_cnt_blk as u16, cmd);
            if st.acpi.pm1b_cnt_blk != 0 {
                let cmd_b = ((st.acpi.slp_typb & 0x07) << 10) | (1 << 13);
                outw(st.acpi.pm1b_cnt_blk as u16, cmd_b);
            }
            for _ in 0..2_000_000u64 {
                core::hint::spin_loop();
            }
        }

        // Tier 3: UEFI Runtime Services ResetSystem(EfiResetShutdown)
        if let Some(reset_fn) = st.uefi_reset_fn {
            crate::kprintln!("[CELEBRON-POWER] Invoking UEFI Runtime ResetSystem(EfiResetShutdown)...");
            reset_fn(EFI_RESET_SHUTDOWN, 0, 0, core::ptr::null());
        }

        // Tier 4: Hypervisor / Emulator fallback ports (QEMU, VirtualBox, Bochs)
        outw(0x0604, 0x2000); // QEMU / KVM ACPI power-off
        outw(0x4004, 0x3400); // VirtualBox ACPI power-off
        outw(0xB004, 0x2000); // Bochs / older QEMU

        crate::kprintln!("[CELEBRON-POWER] System halted (safe to power off).");
        loop {
            asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Execute clean hardware Reboot (Cold or Warm, never returns).
pub fn execute_reboot(warm: bool) -> ! {
    crate::kprintln!(
        "[CELEBRON-POWER] Executing clean system {} REBOOT...",
        if warm { "WARM" } else { "COLD" }
    );
    unsafe {
        asm!("cli", options(nomem, nostack, preserves_flags));
        let st = &*addr_of_mut!(POWER_STATE);

        // Tier 1: ACPI FADT RESET_REG
        if st.acpi.reset_reg_supported && st.acpi.reset_reg.address != 0 {
            crate::kprintln!("[CELEBRON-POWER] Writing ACPI FADT RESET_REG...");
            write_gas_u8(st.acpi.reset_reg, st.acpi.reset_value);
            for _ in 0..2_000_000u64 {
                core::hint::spin_loop();
            }
        }

        // Tier 2: UEFI Runtime Services ResetSystem(EfiResetCold / EfiResetWarm)
        if let Some(reset_fn) = st.uefi_reset_fn {
            let reset_ty = if warm { EFI_RESET_WARM } else { EFI_RESET_COLD };
            crate::kprintln!("[CELEBRON-POWER] Invoking UEFI Runtime ResetSystem({})...", reset_ty);
            reset_fn(reset_ty, 0, 0, core::ptr::null());
        }

        // Tier 3: PCI Reset Control Register (0xCF9)
        outb(0x0CF9, 0x02);
        outb(0x0CF9, if warm { 0x06 } else { 0x0E });

        // Tier 4: Legacy 8042 Keyboard Controller CPU Reset pulse
        let mut guard = 0usize;
        while (inb(0x64) & 0x02) != 0 && guard < 10_000 {
            guard += 1;
        }
        outb(0x64, 0xFE);

        // Tier 5: Intentional Triple Fault fallback
        let null_idt: [u8; 10] = [0; 10];
        asm!(
            "lidt [{}]",
            "int3",
            in(reg) null_idt.as_ptr(),
            options(nostack)
        );

        loop {
            asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}
