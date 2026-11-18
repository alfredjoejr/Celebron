#![no_std]
#![no_main]

mod font;
mod serial;

use celebron_boot_info::{BootInfo, MemoryRegionKind, BOOT_INFO_MAGIC};
use core::arch::asm;
use core::panic::PanicInfo;
use font::{pack_rgb, FramebufferCanvas};

/// Render the Celebron OS Ring-0 Pure Microkernel visual boot dashboard onto the UEFI GOP framebuffer.
fn render_boot_dashboard(boot_info: &BootInfo, usable_regions: usize) {
    if boot_info.has_framebuffer == 0 {
        return;
    }

    let fb = boot_info.framebuffer;
    let canvas = FramebufferCanvas::new(fb);

    let bg_color = pack_rgb(0x0B, 0x10, 0x1B, fb.format);
    let panel_bg = pack_rgb(0x13, 0x1C, 0x2E, fb.format);
    let accent_cyan = pack_rgb(0x00, 0xD4, 0xFF, fb.format);
    let status_green = pack_rgb(0x10, 0xB9, 0x81, fb.format);
    let text_white = pack_rgb(0xF1, 0xF5, 0xF9, fb.format);
    let text_muted = pack_rgb(0x94, 0xA3, 0xB8, fb.format);

    // 1. Clear full background + top cyan accent bar
    canvas.fill_rect(0, 0, fb.width, fb.height, bg_color);
    canvas.fill_rect(0, 0, fb.width, 8, accent_cyan);

    // 2. Header panel
    let card_w = fb.width.saturating_sub(80);
    canvas.fill_rect(40, 36, card_w, 96, panel_bg);
    canvas.fill_rect(40, 36, 8, 96, accent_cyan);

    canvas.draw_str(68, 52, "CELEBRON OS V0.1 - PURE MICROKERNEL (RING 0)", accent_cyan, 2);
    canvas.draw_str(
        68,
        92,
        "PROPRIETARY 64-BIT CAPABILITY ARCHITECTURE | X86_64 LONG MODE",
        text_white,
        1,
    );

    // 3. Telemetry panel
    let body_h = fb.height.saturating_sub(200).min(340);
    canvas.fill_rect(40, 156, card_w, body_h, panel_bg);
    canvas.fill_rect(40, 156, 8, body_h, status_green);

    let mut y = 182;
    let line_step = 34;

    // Row 1: UEFI Bootloader hand-off
    let mut x = canvas.draw_str(68, y, "[OK] CUSTOM UEFI BOOTLOADER HAND-OFF: ABI V", status_green, 1);
    x = canvas.draw_u64_dec(x, y, boot_info.version as u64, text_white, 1);
    canvas.draw_str(x, y, " VERIFIED", text_white, 1);
    y += line_step;

    // Row 2: Magic signature
    x = canvas.draw_str(68, y, "[OK] BOOTINFO SIGNATURE: CELEBRON (", status_green, 1);
    x = canvas.draw_u64_hex(x, y, boot_info.magic, accent_cyan, 1);
    canvas.draw_str(x, y, ")", status_green, 1);
    y += line_step;

    // Row 3: GOP Framebuffer
    x = canvas.draw_str(68, y, "[OK] UEFI GOP FRAMEBUFFER: ", status_green, 1);
    x = canvas.draw_u64_dec(x, y, fb.width as u64, text_white, 1);
    x = canvas.draw_str(x, y, "X", text_white, 1);
    x = canvas.draw_u64_dec(x, y, fb.height as u64, text_white, 1);
    x = canvas.draw_str(x, y, " @ ", text_muted, 1);
    canvas.draw_u64_hex(x, y, fb.base_address, accent_cyan, 1);
    y += line_step;

    // Row 4: ACPI RSDP
    x = canvas.draw_str(68, y, "[OK] ACPI 2.0 RSDP TABLE ADDRESS: ", status_green, 1);
    canvas.draw_u64_hex(x, y, boot_info.acpi_rsdp_address, accent_cyan, 1);
    y += line_step;

    // Row 5: Usable RAM
    let usable_mib = boot_info.total_usable_ram_bytes / (1024 * 1024);
    x = canvas.draw_str(68, y, "[OK] USABLE PHYSICAL RAM: ", status_green, 1);
    x = canvas.draw_u64_dec(x, y, usable_mib, text_white, 1);
    x = canvas.draw_str(x, y, " MIB (", text_white, 1);
    x = canvas.draw_u64_dec(x, y, boot_info.memory_region_count as u64, accent_cyan, 1);
    x = canvas.draw_str(x, y, " REGIONS / ", text_muted, 1);
    x = canvas.draw_u64_dec(x, y, usable_regions as u64, accent_cyan, 1);
    canvas.draw_str(x, y, " RAM RANGES)", text_muted, 1);
    y += line_step + 16;

    // Row 6: Active milestone status
    canvas.draw_str(
        68,
        y,
        "STATUS: MILESTONE 1.1 ONLINE - READY FOR PMM & VMM PAGING",
        accent_cyan,
        1,
    );
}

/// Celebron OS Pure Microkernel Ring-0 Entry Point (`kernel_main`).
/// Invoked directly by the Celebron UEFI Bootloader after `ExitBootServices`.
#[no_mangle]
#[link_section = ".text.kernel_main"]
pub extern "sysv64" fn kernel_main(boot_info: &'static BootInfo) -> ! {
    kprintln!("============================================================");
    kprintln!("[CELEBRON-KERNEL] Celebron OS Pure Microkernel v0.1 (Ring 0)");
    kprintln!("============================================================");

    if boot_info.magic != BOOT_INFO_MAGIC {
        panic!(
            "Invalid BootInfo magic: expected 0x{:016X}, got 0x{:016X}",
            BOOT_INFO_MAGIC, boot_info.magic
        );
    }

    kprintln!(
        "[CELEBRON-KERNEL] BootInfo ABI v{} verified (Magic = 'CELEBRON')",
        boot_info.version
    );
    kprintln!(
        "[CELEBRON-KERNEL] ACPI RSDP physical address: 0x{:016X}",
        boot_info.acpi_rsdp_address
    );

    if boot_info.has_framebuffer != 0 {
        let fb = &boot_info.framebuffer;
        kprintln!(
            "[CELEBRON-KERNEL] GOP Framebuffer active: {}x{} (stride {}), base=0x{:016X}",
            fb.width,
            fb.height,
            fb.pixels_per_scanline,
            fb.base_address
        );
    }

    let mut usable_regions = 0usize;
    let mut reclaimable_bytes = 0u64;
    for i in 0..boot_info.memory_region_count {
        let region = &boot_info.memory_regions[i];
        match region.kind {
            MemoryRegionKind::UsableRam => usable_regions += 1,
            MemoryRegionKind::BootloaderReclaimable => {
                reclaimable_bytes = reclaimable_bytes.saturating_add(region.size_bytes);
            }
            _ => {}
        }
    }

    kprintln!(
        "[CELEBRON-KERNEL] Memory Map: {} total descriptors, {} conventional RAM ranges",
        boot_info.memory_region_count,
        usable_regions
    );
    kprintln!(
        "[CELEBRON-KERNEL] Total Usable + Reclaimable RAM: {} MiB ({} KiB reclaimable boot services)",
        boot_info.total_usable_ram_bytes / (1024 * 1024),
        reclaimable_bytes / 1024
    );

    render_boot_dashboard(boot_info, usable_regions);
    kprintln!("[CELEBRON-KERNEL] Milestone 1.1 Boot Hand-off Complete. Halting CPU cleanly.");

    loop {
        unsafe {
            asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!("\n[CELEBRON-KERNEL RING-0 PANIC] {}", info);
    loop {
        unsafe {
            asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}
