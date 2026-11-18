//! Proprietary clean-room UEFI 2.x specification bindings for Celebron OS (`x86_64-unknown-uefi`).
//! Zero external dependencies.

#![allow(dead_code)]

use core::ffi::c_void;

pub type EfiStatus = usize;
pub type EfiHandle = *mut c_void;
pub type EfiPhysicalAddress = u64;
pub type EfiVirtualAddress = u64;

pub const EFI_SUCCESS: EfiStatus = 0;
pub const EFI_BUFFER_TOO_SMALL: EfiStatus = (1usize << 63) | 5;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EfiGuid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

/// UEFI Graphics Output Protocol (GOP) GUID: `9042a9de-23dc-4a38-96fb-7aded080516a`
pub const EFI_GRAPHICS_OUTPUT_PROTOCOL_GUID: EfiGuid = EfiGuid {
    data1: 0x9042a9de,
    data2: 0x23dc,
    data3: 0x4a38,
    data4: [0x96, 0xfb, 0x7a, 0xde, 0xd0, 0x80, 0x51, 0x6a],
};

/// ACPI 2.0+ RSDP Table GUID: `8868e871-e4f1-11d3-bc22-0080c73c8881`
pub const EFI_ACPI_20_TABLE_GUID: EfiGuid = EfiGuid {
    data1: 0x8868e871,
    data2: 0xe4f1,
    data3: 0x11d3,
    data4: [0xbc, 0x22, 0x00, 0x80, 0xc7, 0x3c, 0x88, 0x81],
};

/// ACPI 1.0 RSDP Table GUID: `eb9d2d30-2d88-11d3-9a16-0090273fc14d`
pub const EFI_ACPI_10_TABLE_GUID: EfiGuid = EfiGuid {
    data1: 0xeb9d2d30,
    data2: 0x2d88,
    data3: 0x11d3,
    data4: [0x9a, 0x16, 0x00, 0x90, 0x27, 0x3f, 0xc1, 0x4d],
};

/// UEFI Simple File System Protocol GUID: `0964e5b22-6459-11d2-8e39-00a0c969723b`
pub const EFI_SIMPLE_FILE_SYSTEM_PROTOCOL_GUID: EfiGuid = EfiGuid {
    data1: 0x0964e5b22,
    data2: 0x6459,
    data3: 0x11d2,
    data4: [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
};

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct EfiTableHeader {
    pub signature: u64,
    pub revision: u32,
    pub header_size: u32,
    pub crc32: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct EfiConfigurationTable {
    pub vendor_guid: EfiGuid,
    pub vendor_table: *const c_void,
}

#[repr(C)]
pub struct EfiSystemTable {
    pub hdr: EfiTableHeader,
    pub firmware_vendor: *const u16,
    pub firmware_revision: u32,
    pub console_in_handle: EfiHandle,
    pub con_in: *mut c_void,
    pub console_out_handle: EfiHandle,
    pub con_out: *mut EfiSimpleTextOutputProtocol,
    pub standard_error_handle: EfiHandle,
    pub std_err: *mut EfiSimpleTextOutputProtocol,
    pub runtime_services: *mut c_void,
    pub boot_services: *mut EfiBootServices,
    pub number_of_table_entries: usize,
    pub configuration_table: *const EfiConfigurationTable,
}

#[repr(C)]
pub struct EfiSimpleTextOutputProtocol {
    pub reset: extern "efiapi" fn(*mut EfiSimpleTextOutputProtocol, bool) -> EfiStatus,
    pub output_string: extern "efiapi" fn(*mut EfiSimpleTextOutputProtocol, *const u16) -> EfiStatus,
    pub test_string: *const c_void,
    pub query_mode: *const c_void,
    pub set_mode: *const c_void,
    pub set_attribute: extern "efiapi" fn(*mut EfiSimpleTextOutputProtocol, usize) -> EfiStatus,
    pub clear_screen: extern "efiapi" fn(*mut EfiSimpleTextOutputProtocol) -> EfiStatus,
}

/// UEFI Memory Allocation Types
pub const ALLOCATE_ANY_PAGES: u32 = 0;
pub const ALLOCATE_MAX_ADDRESS: u32 = 1;
pub const ALLOCATE_ADDRESS: u32 = 2;

/// UEFI Memory Types
pub const EFI_RESERVED_MEMORY_TYPE: u32 = 0;
pub const EFI_LOADER_CODE: u32 = 1;
pub const EFI_LOADER_DATA: u32 = 2;
pub const EFI_BOOT_SERVICES_CODE: u32 = 3;
pub const EFI_BOOT_SERVICES_DATA: u32 = 4;
pub const EFI_RUNTIME_SERVICES_CODE: u32 = 5;
pub const EFI_RUNTIME_SERVICES_DATA: u32 = 6;
pub const EFI_CONVENTIONAL_MEMORY: u32 = 7;
pub const EFI_UNUSABLE_MEMORY: u32 = 8;
pub const EFI_ACPI_RECLAIM_MEMORY: u32 = 9;
pub const EFI_ACPI_MEMORY_NVS: u32 = 10;
pub const EFI_MEMORY_MAPPED_IO: u32 = 11;
pub const EFI_MEMORY_MAPPED_IO_PORT_SPACE: u32 = 12;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct EfiMemoryDescriptor {
    pub ty: u32,
    pub _pad: u32,
    pub physical_start: EfiPhysicalAddress,
    pub virtual_start: EfiVirtualAddress,
    pub number_of_pages: u64,
    pub attribute: u64,
}

#[repr(C)]
pub struct EfiBootServices {
    pub hdr: EfiTableHeader,
    // Task Priority Services
    pub raise_tpl: *const c_void,
    pub restore_tpl: *const c_void,
    // Memory Services
    pub allocate_pages: extern "efiapi" fn(
        alloc_type: u32,
        mem_type: u32,
        pages: usize,
        memory: *mut EfiPhysicalAddress,
    ) -> EfiStatus,
    pub free_pages: extern "efiapi" fn(memory: EfiPhysicalAddress, pages: usize) -> EfiStatus,
    pub get_memory_map: extern "efiapi" fn(
        memory_map_size: *mut usize,
        memory_map: *mut u8,
        map_key: *mut usize,
        descriptor_size: *mut usize,
        descriptor_version: *mut u32,
    ) -> EfiStatus,
    pub allocate_pool: extern "efiapi" fn(
        pool_type: u32,
        size: usize,
        buffer: *mut *mut u8,
    ) -> EfiStatus,
    pub free_pool: extern "efiapi" fn(buffer: *mut u8) -> EfiStatus,
    // Event & Timer Services
    pub create_event: *const c_void,
    pub set_timer: *const c_void,
    pub wait_for_event: *const c_void,
    pub signal_event: *const c_void,
    pub close_event: *const c_void,
    pub check_event: *const c_void,
    // Protocol Handler Services
    pub install_protocol_interface: *const c_void,
    pub reinstall_protocol_interface: *const c_void,
    pub uninstall_protocol_interface: *const c_void,
    pub handle_protocol: extern "efiapi" fn(
        handle: EfiHandle,
        protocol: *const EfiGuid,
        interface: *mut *mut c_void,
    ) -> EfiStatus,
    pub reserved: *const c_void,
    pub register_protocol_notify: *const c_void,
    pub locate_handle: *const c_void,
    pub locate_device_path: *const c_void,
    pub install_configuration_table: *const c_void,
    // Image Services
    pub load_image: *const c_void,
    pub start_image: *const c_void,
    pub exit: *const c_void,
    pub unload_image: *const c_void,
    pub exit_boot_services: extern "efiapi" fn(
        image_handle: EfiHandle,
        map_key: usize,
    ) -> EfiStatus,
    // Miscellaneous Services
    pub get_next_monotonic_count: *const c_void,
    pub stall: extern "efiapi" fn(microseconds: usize) -> EfiStatus,
    pub set_watchdog_timer: extern "efiapi" fn(
        timeout: usize,
        watchdog_code: u64,
        data_size: usize,
        watchdog_data: *const u16,
    ) -> EfiStatus,
    // DriverSupport Services
    pub connect_controller: *const c_void,
    pub disconnect_controller: *const c_void,
    // Open and Close Protocol Services
    pub open_protocol: *const c_void,
    pub close_protocol: *const c_void,
    pub open_protocol_information: *const c_void,
    // Library Services
    pub protocols_per_handle: *const c_void,
    pub locate_handle_buffer: *const c_void,
    pub locate_protocol: extern "efiapi" fn(
        protocol: *const EfiGuid,
        registration: *const c_void,
        interface: *mut *mut c_void,
    ) -> EfiStatus,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EfiGraphicsPixelFormat {
    PixelRedGreenBlueReserved8BitPerColor = 0,
    PixelBlueGreenRedReserved8BitPerColor = 1,
    PixelBitMask = 2,
    PixelBltOnly = 3,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct EfiGraphicsOutputModeInformation {
    pub version: u32,
    pub horizontal_resolution: u32,
    pub vertical_resolution: u32,
    pub pixel_format: EfiGraphicsPixelFormat,
    pub pixel_information: [u32; 4],
    pub pixels_per_scan_line: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct EfiGraphicsOutputProtocolMode {
    pub max_mode: u32,
    pub mode: u32,
    pub info: *const EfiGraphicsOutputModeInformation,
    pub size_of_info: usize,
    pub frame_buffer_base: EfiPhysicalAddress,
    pub frame_buffer_size: usize,
}

#[repr(C)]
pub struct EfiGraphicsOutputProtocol {
    pub query_mode: extern "efiapi" fn(
        this: *mut EfiGraphicsOutputProtocol,
        mode_number: u32,
        size_of_info: *mut usize,
        info: *mut *const EfiGraphicsOutputModeInformation,
    ) -> EfiStatus,
    pub set_mode: extern "efiapi" fn(
        this: *mut EfiGraphicsOutputProtocol,
        mode_number: u32,
    ) -> EfiStatus,
    pub blt: *const c_void,
    pub mode: *const EfiGraphicsOutputProtocolMode,
}

/// Simple File System Protocol for loading `\EFI\CELEBRON\KERNEL.ELF` from the ESP.
#[repr(C)]
pub struct EfiSimpleFileSystemProtocol {
    pub revision: u64,
    pub open_volume: extern "efiapi" fn(
        this: *mut EfiSimpleFileSystemProtocol,
        root: *mut *mut EfiFileProtocol,
    ) -> EfiStatus,
}

pub const EFI_FILE_MODE_READ: u64 = 0x0000_0000_0000_0001;

#[repr(C)]
pub struct EfiFileProtocol {
    pub revision: u64,
    pub open: extern "efiapi" fn(
        this: *mut EfiFileProtocol,
        new_handle: *mut *mut EfiFileProtocol,
        file_name: *const u16,
        open_mode: u64,
        attributes: u64,
    ) -> EfiStatus,
    pub close: extern "efiapi" fn(this: *mut EfiFileProtocol) -> EfiStatus,
    pub delete: *const c_void,
    pub read: extern "efiapi" fn(
        this: *mut EfiFileProtocol,
        buffer_size: *mut usize,
        buffer: *mut u8,
    ) -> EfiStatus,
}
