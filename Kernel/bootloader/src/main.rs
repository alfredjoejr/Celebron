#![no_std]
#![no_main]

mod elf;
mod serial;
mod uefi_raw;

use celebron_boot_info::{
    BootInfo, FramebufferInfo, KernelEntryFn, MemoryRegion, MemoryRegionKind, PixelFormat,
    MAX_MEMORY_REGIONS,
};
use core::arch::asm;
use core::ffi::c_void;
use core::panic::PanicInfo;
use uefi_raw::*;

/// Dedicated 64 KiB aligned kernel stack allocated inside the bootloader image
/// for the initial transition into the Celebron Microkernel (`kernel_main`).
#[repr(C, align(4096))]
struct KernelBootStack([u8; 64 * 1024]);

static mut KERNEL_BOOT_STACK: KernelBootStack = KernelBootStack([0; 64 * 1024]);

/// Global static `BootInfo` structure populated prior to `ExitBootServices` and passed to the kernel.
static mut BOOT_INFO: BootInfo = BootInfo::new();

/// Temporary buffer for reading `\EFI\CELEBRON\KERNEL.ELF` from the EFI System Partition (up to 2 MiB).
#[repr(C, align(4096))]
struct KernelFileBuffer([u8; 2 * 1024 * 1024]);

static mut KERNEL_FILE_BUFFER: KernelFileBuffer = KernelFileBuffer([0; 2 * 1024 * 1024]);

/// Buffer for the raw UEFI Memory Map descriptors during `GetMemoryMap` and `ExitBootServices`.
#[repr(C, align(16))]
struct UefiMemoryMapBuffer([u8; 32 * 1024]);

static mut UEFI_MEMMAP_BUFFER: UefiMemoryMapBuffer = UefiMemoryMapBuffer([0; 32 * 1024]);

/// Print an ASCII string to the UEFI `ConOut` text console (converting ASCII to UCS-2 on the stack).
unsafe fn uefi_print(con_out: *mut EfiSimpleTextOutputProtocol, msg: &str) {
    if con_out.is_null() {
        return;
    }
    let mut buf = [0u16; 128];
    let mut idx = 0usize;

    for b in msg.bytes() {
        if b == b'\n' {
            if idx + 2 >= buf.len() {
                buf[idx] = 0;
                ((*con_out).output_string)(con_out, buf.as_ptr());
                idx = 0;
            }
            buf[idx] = b'\r' as u16;
            buf[idx + 1] = b'\n' as u16;
            idx += 2;
        } else {
            if idx + 1 >= buf.len() {
                buf[idx] = 0;
                ((*con_out).output_string)(con_out, buf.as_ptr());
                idx = 0;
            }
            buf[idx] = b as u16;
            idx += 1;
        }
    }

    if idx > 0 {
        buf[idx] = 0;
        ((*con_out).output_string)(con_out, buf.as_ptr());
    }
}

/// Locate the ACPI 2.0 (or 1.0 fallback) RSDP physical address from the UEFI Configuration Tables.
unsafe fn find_acpi_rsdp(system_table: &EfiSystemTable) -> u64 {
    let count = system_table.number_of_table_entries;
    let tables = system_table.configuration_table;
    if tables.is_null() || count == 0 {
        return 0;
    }

    let mut acpi_10_fallback: u64 = 0;
    for i in 0..count {
        let entry = *tables.add(i);
        if entry.vendor_guid == EFI_ACPI_20_TABLE_GUID {
            return entry.vendor_table as u64;
        } else if entry.vendor_guid == EFI_ACPI_10_TABLE_GUID {
            acpi_10_fallback = entry.vendor_table as u64;
        }
    }
    acpi_10_fallback
}

/// Locate and configure the UEFI Graphics Output Protocol (GOP) linear framebuffer.
unsafe fn init_gop_framebuffer(boot_services: &EfiBootServices) -> Option<FramebufferInfo> {
    let mut gop_ptr: *mut c_void = core::ptr::null_mut();
    let status = (boot_services.locate_protocol)(
        &EFI_GRAPHICS_OUTPUT_PROTOCOL_GUID,
        core::ptr::null(),
        &mut gop_ptr,
    );

    if status != EFI_SUCCESS || gop_ptr.is_null() {
        return None;
    }

    let gop = gop_ptr as *mut EfiGraphicsOutputProtocol;
    let mode_ptr = (*gop).mode;
    if mode_ptr.is_null() {
        return None;
    }

    let mode = *mode_ptr;
    if mode.info.is_null() {
        return None;
    }
    let info = *mode.info;

    let format = match info.pixel_format {
        EfiGraphicsPixelFormat::PixelBlueGreenRedReserved8BitPerColor => PixelFormat::Bgr,
        EfiGraphicsPixelFormat::PixelRedGreenBlueReserved8BitPerColor => PixelFormat::Rgb,
        EfiGraphicsPixelFormat::PixelBitMask => PixelFormat::Bitmask,
        EfiGraphicsPixelFormat::PixelBltOnly => return None,
    };

    Some(FramebufferInfo {
        base_address: mode.frame_buffer_base,
        buffer_size: mode.frame_buffer_size,
        width: info.horizontal_resolution as usize,
        height: info.vertical_resolution as usize,
        pixels_per_scanline: info.pixels_per_scan_line as usize,
        format,
    })
}

/// Attempt to load `\EFI\CELEBRON\KERNEL.ELF` from the boot volume via UEFI Simple File System Protocol.
unsafe fn try_load_kernel_file_from_esp(
    boot_services: &EfiBootServices,
) -> Option<&'static [u8]> {
    let mut sfs_ptr: *mut c_void = core::ptr::null_mut();
    let status = (boot_services.locate_protocol)(
        &EFI_SIMPLE_FILE_SYSTEM_PROTOCOL_GUID,
        core::ptr::null(),
        &mut sfs_ptr,
    );
    if status != EFI_SUCCESS || sfs_ptr.is_null() {
        return None;
    }

    let sfs = sfs_ptr as *mut EfiSimpleFileSystemProtocol;
    let mut root: *mut EfiFileProtocol = core::ptr::null_mut();
    if ((*sfs).open_volume)(sfs, &mut root) != EFI_SUCCESS || root.is_null() {
        return None;
    }

    // UCS-2 null-terminated path: `\EFI\CELEBRON\KERNEL.ELF`
    const KERNEL_PATH: &[u16] = &[
        b'\\' as u16, b'E' as u16, b'F' as u16, b'I' as u16,
        b'\\' as u16, b'C' as u16, b'E' as u16, b'L' as u16, b'E' as u16, b'B' as u16,
        b'R' as u16, b'O' as u16, b'N' as u16,
        b'\\' as u16, b'K' as u16, b'E' as u16, b'R' as u16, b'N' as u16, b'E' as u16,
        b'L' as u16, b'.' as u16, b'E' as u16, b'L' as u16, b'F' as u16, 0,
    ];

    let mut kernel_file: *mut EfiFileProtocol = core::ptr::null_mut();
    let open_status = ((*root).open)(
        root,
        &mut kernel_file,
        KERNEL_PATH.as_ptr(),
        EFI_FILE_MODE_READ,
        0,
    );

    if open_status != EFI_SUCCESS || kernel_file.is_null() {
        let _ = ((*root).close)(root);
        return None;
    }

    let buf_ptr = core::ptr::addr_of_mut!(KERNEL_FILE_BUFFER.0) as *mut u8;
    let mut read_size = core::mem::size_of::<KernelFileBuffer>();
    let read_status = ((*kernel_file).read)(kernel_file, &mut read_size, buf_ptr);

    let _ = ((*kernel_file).close)(kernel_file);
    let _ = ((*root).close)(root);

    if read_status != EFI_SUCCESS || read_size == 0 {
        return None;
    }

    Some(core::slice::from_raw_parts(buf_ptr, read_size))
}

/// Convert a UEFI memory type into Celebron's normalized [`MemoryRegionKind`].
fn classify_uefi_memory_type(uefi_type: u32) -> MemoryRegionKind {
    match uefi_type {
        EFI_CONVENTIONAL_MEMORY => MemoryRegionKind::UsableRam,
        EFI_LOADER_CODE | EFI_LOADER_DATA => MemoryRegionKind::KernelAndModules,
        EFI_BOOT_SERVICES_CODE | EFI_BOOT_SERVICES_DATA => MemoryRegionKind::BootloaderReclaimable,
        EFI_ACPI_RECLAIM_MEMORY => MemoryRegionKind::AcpiReclaimable,
        EFI_ACPI_MEMORY_NVS | EFI_RUNTIME_SERVICES_CODE | EFI_RUNTIME_SERVICES_DATA => {
            MemoryRegionKind::FirmwareReserved
        }
        _ => MemoryRegionKind::MmioOrReserved,
    }
}

/// Query the final UEFI Memory Map, populate `BOOT_INFO`, and invoke `ExitBootServices`.
unsafe fn exit_boot_services_and_populate_memmap(
    image_handle: EfiHandle,
    boot_services: &EfiBootServices,
) -> Result<(), EfiStatus> {
    // Retry up to 4 times in case firmware updates the map key between GetMemoryMap and ExitBootServices.
    for _attempt in 0..4 {
        let buf_ptr = core::ptr::addr_of_mut!(UEFI_MEMMAP_BUFFER.0) as *mut u8;
        let mut map_size = core::mem::size_of::<UefiMemoryMapBuffer>();
        let mut map_key: usize = 0;
        let mut descriptor_size: usize = 0;
        let mut descriptor_version: u32 = 0;

        let status = (boot_services.get_memory_map)(
            &mut map_size,
            buf_ptr,
            &mut map_key,
            &mut descriptor_size,
            &mut descriptor_version,
        );
        if status != EFI_SUCCESS || descriptor_size == 0 {
            return Err(status);
        }

        // Convert descriptors into Celebron's normalized `BOOT_INFO` table prior to exiting boot services.
        let num_descriptors = map_size / descriptor_size;
        let mut count = 0usize;
        let mut total_usable_ram = 0u64;

        for i in 0..num_descriptors {
            if count >= MAX_MEMORY_REGIONS {
                break;
            }
            let desc_ptr = buf_ptr.add(i * descriptor_size) as *const EfiMemoryDescriptor;
            let desc = core::ptr::read_unaligned(desc_ptr);
            let size_bytes = desc.number_of_pages.saturating_mul(4096);
            if size_bytes == 0 {
                continue;
            }

            let kind = classify_uefi_memory_type(desc.ty);
            if matches!(
                kind,
                MemoryRegionKind::UsableRam | MemoryRegionKind::BootloaderReclaimable
            ) {
                total_usable_ram = total_usable_ram.saturating_add(size_bytes);
            }

            BOOT_INFO.memory_regions[count] = MemoryRegion {
                physical_start: desc.physical_start,
                size_bytes,
                kind,
                _reserved: 0,
            };
            count += 1;
        }

        BOOT_INFO.memory_region_count = count;
        BOOT_INFO.total_usable_ram_bytes = total_usable_ram;

        let exit_status = (boot_services.exit_boot_services)(image_handle, map_key);
        if exit_status == EFI_SUCCESS {
            return Ok(());
        }
    }

    Err(EFI_BUFFER_TOO_SMALL)
}

/// Embedded copy of the compiled Celebron Microkernel (`KERNEL.ELF`) so the bootloader
/// can still load the full ELF64 kernel if the firmware's first SimpleFileSystem handle
/// is a virtual CD/DVD drive rather than the ESP.
static EMBEDDED_KERNEL_ELF: &[u8] = include_bytes!("../../build/esp/EFI/CELEBRON/KERNEL.ELF");

/// UEFI Bootloader Entry Point (`efi_main`).
#[no_mangle]
pub extern "efiapi" fn efi_main(
    image_handle: EfiHandle,
    system_table: *const EfiSystemTable,
) -> EfiStatus {
    serial::SerialPort::init();
    serial_println!("============================================================");
    serial_println!("[CELEBRON-BOOT] Celebron OS Proprietary UEFI Bootloader v0.1");
    serial_println!("============================================================");

    if system_table.is_null() {
        return 1;
    }

    unsafe {
        let st = &*system_table;
        let con_out = st.con_out;
        let bs = &*st.boot_services;

        // Disable the 5-minute UEFI hardware watchdog timer so it doesn't reset the system after boot
        let _ = (bs.set_watchdog_timer)(0, 0, 0, core::ptr::null());

        if !con_out.is_null() {
            let _ = ((*con_out).clear_screen)(con_out);
            uefi_print(con_out, "Celebron OS Proprietary UEFI Bootloader v0.1\n");
            uefi_print(con_out, "[1/5] Watchdog disabled & COM1 serial initialized (115200 8N1).\n");
        }

        // 1. Discover ACPI RSDP
        let rsdp_addr = find_acpi_rsdp(st);
        BOOT_INFO.acpi_rsdp_address = rsdp_addr;
        serial_println!("[CELEBRON-BOOT] ACPI RSDP physical address: 0x{:016X}", rsdp_addr);
        uefi_print(con_out, "[2/5] ACPI RSDP table discovered.\n");

        // 2. Discover & initialize UEFI GOP Framebuffer
        if let Some(fb) = init_gop_framebuffer(bs) {
            serial_println!(
                "[CELEBRON-BOOT] GOP Framebuffer: {}x{} (stride {}), base=0x{:016X}, size={} KiB",
                fb.width,
                fb.height,
                fb.pixels_per_scanline,
                fb.base_address,
                fb.buffer_size / 1024
            );
            BOOT_INFO.has_framebuffer = 1;
            BOOT_INFO.framebuffer = fb;
            uefi_print(con_out, "[3/5] UEFI GOP Linear Framebuffer online.\n");
        } else {
            serial_println!("[CELEBRON-BOOT] Warning: No UEFI GOP framebuffer detected.");
            uefi_print(con_out, "[3/5] Warning: No UEFI GOP Framebuffer found (headless mode).\n");
        }

        // 3. Load `\EFI\CELEBRON\KERNEL.ELF` from the EFI System Partition (or embedded ELF64 payload)
        let elf_bytes: &[u8] = if let Some(esp_bytes) = try_load_kernel_file_from_esp(bs) {
            serial_println!(
                "[CELEBRON-BOOT] Found \\EFI\\CELEBRON\\KERNEL.ELF on ESP ({} bytes).",
                esp_bytes.len()
            );
            uefi_print(con_out, "[4/5] Loaded \\EFI\\CELEBRON\\KERNEL.ELF from EFI System Partition.\n");
            esp_bytes
        } else {
            serial_println!(
                "[CELEBRON-BOOT] Loading embedded Celebron Microkernel ELF64 image ({} bytes)...",
                EMBEDDED_KERNEL_ELF.len()
            );
            uefi_print(con_out, "[4/5] Loaded Celebron Microkernel ELF64 image.\n");
            EMBEDDED_KERNEL_ELF
        };

        let loaded = elf::load_elf64_kernel(bs, elf_bytes)
            .expect("Failed to load Celebron Microkernel ELF64 image");
        serial_println!(
            "[CELEBRON-BOOT] Mapped {} ELF64 segments [0x{:016X}..0x{:016X}], entry=0x{:016X}",
            loaded.segment_count,
            loaded.lowest_phys_addr,
            loaded.highest_phys_addr,
            loaded.entry_point
        );
        let kernel_entry_addr = loaded.entry_point;

        uefi_print(con_out, "[5/5] Exiting UEFI Boot Services & jumping to Ring-0 Kernel...\n");
        serial_println!("[CELEBRON-BOOT] Calling ExitBootServices...");

        // 4. Acquire final memory map & exit UEFI Boot Services
        if let Err(status) = exit_boot_services_and_populate_memmap(image_handle, bs) {
            serial_println!("[CELEBRON-BOOT] FATAL: ExitBootServices failed with status 0x{:X}", status);
            loop {
                asm!("hlt", options(nomem, nostack, preserves_flags));
            }
        }

        // 5. Boot services are now terminated! Disable interrupts, switch to our 16-byte aligned
        //    64 KiB Kernel Boot Stack, and jump to the kernel entry point (`sysv64` ABI: rdi = &BOOT_INFO).
        let boot_info_ptr = core::ptr::addr_of!(BOOT_INFO);
        let stack_top = (core::ptr::addr_of!(KERNEL_BOOT_STACK.0) as usize
            + core::mem::size_of::<KernelBootStack>())
            & !0xFusize;

        let kernel_entry: KernelEntryFn = core::mem::transmute(kernel_entry_addr as usize);

        asm!(
            "cli",
            "mov rsp, {stack_top}",
            "xor rbp, rbp",
            stack_top = in(reg) stack_top,
            options(nostack)
        );

        kernel_entry(&*boot_info_ptr);
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    serial_println!("[CELEBRON-BOOT PANIC] {}", info);
    loop {
        unsafe {
            asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}
