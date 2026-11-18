#![no_std]
#![no_main]

extern crate alloc;

mod font;
mod gdt;
mod heap;
mod idt;
mod interrupts;
mod pmm;
mod serial;
mod vmm;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use celebron_boot_info::{BootInfo, MemoryRegionKind, BOOT_INFO_MAGIC};
use core::arch::asm;
use core::panic::PanicInfo;
use font::{pack_rgb, FramebufferCanvas};
use heap::HeapStats;
use interrupts::ApicInfo;
use pmm::PmmStats;
use vmm::VirtualMemoryManager;

/// Render the Celebron OS Ring-0 Pure Microkernel visual boot dashboard onto the UEFI GOP framebuffer.
/// Returns the `(x, y)` coordinates of the live tick counter if a framebuffer is active.
fn render_boot_dashboard(
    boot_info: &BootInfo,
    pmm_stats: &PmmStats,
    vmm: &VirtualMemoryManager,
    heap_stats: &HeapStats,
    alloc_test_addr: u64,
    apic_info: &ApicInfo,
    initial_ticks: u64,
) -> Option<(usize, usize)> {
    if boot_info.has_framebuffer == 0 {
        return None;
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
    let body_h = fb.height.saturating_sub(200).min(480);
    canvas.fill_rect(40, 156, card_w, body_h, panel_bg);
    canvas.fill_rect(40, 156, 8, body_h, status_green);

    let mut y = 182;
    let line_step = 32;

    // Row 1: BootInfo ABI & Signature
    let mut x = canvas.draw_str(68, y, "[OK] UEFI BOOTLOADER: ABI V", status_green, 1);
    x = canvas.draw_u64_dec(x, y, boot_info.version as u64, text_white, 1);
    x = canvas.draw_str(x, y, " VERIFIED | MAGIC: CELEBRON (", text_white, 1);
    x = canvas.draw_u64_hex(x, y, boot_info.magic, accent_cyan, 1);
    canvas.draw_str(x, y, ")", text_white, 1);
    y += line_step;

    // Row 2: GOP Framebuffer & ACPI RSDP
    x = canvas.draw_str(68, y, "[OK] GOP FB: ", status_green, 1);
    x = canvas.draw_u64_dec(x, y, fb.width as u64, text_white, 1);
    x = canvas.draw_str(x, y, "X", text_white, 1);
    x = canvas.draw_u64_dec(x, y, fb.height as u64, text_white, 1);
    x = canvas.draw_str(x, y, " @ ", text_muted, 1);
    x = canvas.draw_u64_hex(x, y, fb.base_address, accent_cyan, 1);
    x = canvas.draw_str(x, y, " | RSDP: ", text_muted, 1);
    canvas.draw_u64_hex(x, y, boot_info.acpi_rsdp_address, accent_cyan, 1);
    y += line_step;

    // Row 3: PMM Frame Allocator
    let free_mib = (pmm_stats.free_frames as u64 * pmm::PAGE_SIZE) / (1024 * 1024);
    let total_mib = boot_info.total_usable_ram_bytes / (1024 * 1024);
    x = canvas.draw_str(68, y, "[OK] PMM BITMAP: ", status_green, 1);
    x = canvas.draw_u64_dec(x, y, pmm_stats.free_frames as u64, text_white, 1);
    x = canvas.draw_str(x, y, " FREE FRAMES (", text_white, 1);
    x = canvas.draw_u64_dec(x, y, free_mib, accent_cyan, 1);
    x = canvas.draw_str(x, y, " MIB AVAIL / ", text_muted, 1);
    x = canvas.draw_u64_dec(x, y, total_mib, text_white, 1);
    canvas.draw_str(x, y, " MIB RAM)", text_muted, 1);
    y += line_step;

    // Row 4: VMM 4-Level Paging CR3
    x = canvas.draw_str(68, y, "[OK] VMM PAGING CR3: ", status_green, 1);
    x = canvas.draw_u64_hex(x, y, vmm.pml4_phys, accent_cyan, 1);
    x = canvas.draw_str(x, y, " (", text_muted, 1);
    x = canvas.draw_u64_dec(x, y, vmm.page_tables_allocated as u64, text_white, 1);
    x = canvas.draw_str(x, y, " TABLES, ", text_muted, 1);
    x = canvas.draw_u64_dec(x, y, vmm.mapped_heap_pages as u64, text_white, 1);
    canvas.draw_str(x, y, " HEAP PAGES)", text_muted, 1);
    y += line_step;

    // Row 5: Higher-Half Direct Physical Map verification
    x = canvas.draw_str(68, y, "[OK] HIGHER-HALF DIRECT MAP: ", status_green, 1);
    x = canvas.draw_u64_hex(x, y, vmm::HIGHER_HALF_PHYS_OFFSET, accent_cyan, 1);
    canvas.draw_str(x, y, " -> BOOTINFO OK", text_white, 1);
    y += line_step;

    // Row 6: Ring-0 Kernel Heap
    x = canvas.draw_str(68, y, "[OK] KERNEL HEAP: ", status_green, 1);
    x = canvas.draw_u64_dec(x, y, (heap_stats.total_bytes / 1024) as u64, text_white, 1);
    x = canvas.draw_str(x, y, " KIB @ ", text_white, 1);
    x = canvas.draw_u64_hex(x, y, vmm::KERNEL_HEAP_VIRT_START, accent_cyan, 1);
    x = canvas.draw_str(x, y, " (USED ", text_muted, 1);
    x = canvas.draw_u64_dec(x, y, heap_stats.used_bytes as u64, text_white, 1);
    canvas.draw_str(x, y, " B)", text_muted, 1);
    y += line_step;

    // Row 7: Alloc self-test verification
    x = canvas.draw_str(68, y, "[OK] ALLOC SELF-TEST: BOX/VEC(64)/STRING OK @ ", status_green, 1);
    canvas.draw_u64_hex(x, y, alloc_test_addr, accent_cyan, 1);
    y += line_step;

    // Row 8: GDT / TSS / IDT verification
    canvas.draw_str(
        68,
        y,
        "[OK] GDT/TSS/IDT: CS=0X08 TSS=0X28 (IST1 #DF) | #BP INT3 TEST PASSED",
        status_green,
        1,
    );
    y += line_step;

    // Row 9: Local APIC Timer & Live Ticks
    let mode_label = if apic_info.is_x2apic {
        "X2APIC"
    } else {
        "XAPIC"
    };
    x = canvas.draw_str(68, y, "[OK] LOCAL APIC TIMER (VEC 32): ", status_green, 1);
    x = canvas.draw_str(x, y, mode_label, text_white, 1);
    let tick_x = canvas.draw_str(x, y, " ONLINE | LIVE TICKS: ", text_muted, 1);
    let tick_y = y;
    canvas.draw_u64_dec(tick_x, tick_y, initial_ticks, accent_cyan, 1);
    y += line_step + 16;

    // Row 10: Active milestone status
    canvas.draw_str(
        68,
        y,
        "STATUS: MILESTONE 1.3 ONLINE - GDT, TSS, IDT & APIC TIMER ACTIVE",
        accent_cyan,
        1,
    );

    Some((tick_x, tick_y))
}

/// Update only the numeric tick counter on the GOP dashboard without full-screen flicker.
fn render_live_tick_counter(boot_info: &BootInfo, tick_x: usize, tick_y: usize, ticks: u64) {
    if boot_info.has_framebuffer == 0 {
        return;
    }
    let fb = boot_info.framebuffer;
    let canvas = FramebufferCanvas::new(fb);
    let panel_bg = pack_rgb(0x13, 0x1C, 0x2E, fb.format);
    let accent_cyan = pack_rgb(0x00, 0xD4, 0xFF, fb.format);

    canvas.fill_rect(tick_x, tick_y, 200, 18, panel_bg);
    canvas.draw_u64_dec(tick_x, tick_y, ticks, accent_cyan, 1);
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
        "[CELEBRON-KERNEL] Memory Map: {} total descriptors, {} conventional RAM ranges ({} MiB usable, {} KiB reclaimable)",
        boot_info.memory_region_count,
        usable_regions,
        boot_info.total_usable_ram_bytes / (1024 * 1024),
        reclaimable_bytes / 1024
    );

    // 1. Initialize Physical Memory Manager (PMM)
    let mut pmm = pmm::PhysicalMemoryManager::init(boot_info);
    let pmm_init_stats = pmm.stats();
    kprintln!(
        "[CELEBRON-PMM] Initialized: {} usable frames ({} MiB free above 32 MiB floor, highest=0x{:016X})",
        pmm_init_stats.total_usable_frames,
        (pmm_init_stats.free_frames as u64 * pmm::PAGE_SIZE) / (1024 * 1024),
        pmm_init_stats.highest_phys_addr
    );

    // 2. Initialize Virtual Memory Manager (VMM) and switch CR3
    let vmm = vmm::VirtualMemoryManager::init(&mut pmm, boot_info);
    kprintln!(
        "[CELEBRON-VMM] Active CR3=0x{:016X} ({} page tables, {} heap pages mapped @ 0x{:016X})",
        vmm.pml4_phys,
        vmm.page_tables_allocated,
        vmm.mapped_heap_pages,
        vmm::KERNEL_HEAP_VIRT_START
    );

    // 3. Verify Higher-Half Direct Physical Map access via new CR3
    let hh_boot_info_ptr =
        (vmm::HIGHER_HALF_PHYS_OFFSET + (boot_info as *const BootInfo as u64)) as *const u64;
    let hh_magic = unsafe { hh_boot_info_ptr.read_volatile() };
    if hh_magic != BOOT_INFO_MAGIC {
        panic!(
            "Higher-half direct map verification failed: expected 0x{:016X}, got 0x{:016X}",
            BOOT_INFO_MAGIC, hh_magic
        );
    }
    kprintln!(
        "[CELEBRON-VMM] Higher-half direct map verified: 0x{:016X} -> BootInfo OK",
        vmm::HIGHER_HALF_PHYS_OFFSET
    );

    // 4. Initialize Ring-0 Kernel Heap
    heap::init_kernel_heap(
        vmm::KERNEL_HEAP_VIRT_START as usize,
        vmm::KERNEL_HEAP_SIZE_BYTES,
    );
    kprintln!(
        "[CELEBRON-HEAP] Initialized {} KiB kernel heap at 0x{:016X}",
        vmm::KERNEL_HEAP_SIZE_BYTES / 1024,
        vmm::KERNEL_HEAP_VIRT_START
    );

    // 5. Run Ring-0 `alloc` self-test (Box, Vec, String)
    let heap_stats_live;
    let alloc_test_addr: u64;
    {
        let boxed = Box::new(0xCE1E_B201_2026_0001u64);
        alloc_test_addr = (&*boxed as *const u64) as u64;
        assert!(
            *boxed == 0xCE1E_B201_2026_0001u64 && alloc_test_addr >= vmm::KERNEL_HEAP_VIRT_START,
            "Box heap allocation test failed"
        );

        let mut vec = Vec::with_capacity(64);
        for i in 0u64..64 {
            vec.push(i * i);
        }
        let vec_addr = vec.as_ptr() as u64;
        assert!(
            vec.len() == 64 && vec[63] == 63 * 63 && vec_addr >= vmm::KERNEL_HEAP_VIRT_START,
            "Vec heap allocation test failed"
        );

        let label = String::from("CELEBRON-RING0-HEAP-OK");
        let str_addr = label.as_ptr() as u64;
        assert!(
            label == "CELEBRON-RING0-HEAP-OK" && str_addr >= vmm::KERNEL_HEAP_VIRT_START,
            "String heap allocation test failed"
        );

        heap_stats_live = heap::heap_stats();
        kprintln!(
            "[CELEBRON-HEAP] Self-test passed: Box=0x{:016X}, Vec=0x{:016X}, String=0x{:016X} (used={} B, active={})",
            alloc_test_addr,
            vec_addr,
            str_addr,
            heap_stats_live.used_bytes,
            heap_stats_live.active_allocations
        );
    }

    let heap_stats_after_drop = heap::heap_stats();
    assert!(
        heap_stats_after_drop.active_allocations == 0 && heap_stats_after_drop.used_bytes == 0,
        "Kernel heap leak detected after drop"
    );
    kprintln!(
        "[CELEBRON-HEAP] Deallocation & reclaim verified (active={}, used={} B, free={} KiB)",
        heap_stats_after_drop.active_allocations,
        heap_stats_after_drop.used_bytes,
        heap_stats_after_drop.free_bytes / 1024
    );

    // 6. Initialize 64-bit GDT & TSS (`RSP0` + `IST1` Double-Fault stack)
    gdt::init();
    kprintln!(
        "[CELEBRON-GDT] Loaded 64-bit GDT & TSS (CS=0x{:02X}, DS=0x{:02X}, TR=0x{:02X}, IST1=#DF)",
        gdt::KERNEL_CODE_SELECTOR,
        gdt::KERNEL_DATA_SELECTOR,
        gdt::TSS_SELECTOR
    );

    // 7. Initialize 64-bit IDT & verify `#BP` (int3) trap handling
    idt::init();
    let bp_count = idt::run_breakpoint_self_test();
    kprintln!(
        "[CELEBRON-IDT] 256-gate IDT active; #BP int3 self-test passed (count={})",
        bp_count
    );

    // 8. Mask 8259 PIC, start Local APIC periodic timer (Vec 32), and enable interrupts (`sti`)
    let apic_info = interrupts::init_apic_and_timer();
    let initial_ticks = interrupts::enable_interrupts_and_verify_timer();
    kprintln!(
        "[CELEBRON-APIC] {} Timer Online (base=0x{:016X}, vec={}, initial_ticks={})",
        if apic_info.is_x2apic { "x2APIC" } else { "xAPIC" },
        apic_info.base_addr,
        interrupts::TIMER_INTERRUPT_VECTOR,
        initial_ticks
    );

    let pmm_final_stats = pmm.stats();
    kprintln!(
        "[CELEBRON-PMM] Final allocation state: {} used frames, {} free frames remaining",
        pmm_final_stats.used_frames,
        pmm_final_stats.free_frames
    );
    let tick_coords = render_boot_dashboard(
        boot_info,
        &pmm_final_stats,
        &vmm,
        &heap_stats_live,
        alloc_test_addr,
        &apic_info,
        initial_ticks,
    );
    kprintln!("[CELEBRON-KERNEL] Milestone 1.3 Online (GDT, TSS, IDT & Local APIC Timer Active).");

    let mut last_rendered_ticks = initial_ticks;
    loop {
        let now_ticks = interrupts::timer_ticks();
        if now_ticks.saturating_sub(last_rendered_ticks) >= 10 {
            if let Some((tx, ty)) = tick_coords {
                render_live_tick_counter(boot_info, tx, ty, now_ticks);
            }
            last_rendered_ticks = now_ticks;
        }
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
