#![no_std]

/// Magic signature verifying a valid Celebron BootInfo structure (`"CELEBRON"` in ASCII).
pub const BOOT_INFO_MAGIC: u64 = 0x4E4F_5242_454C_4543;

/// Version of the Celebron Boot Hand-off ABI.
pub const BOOT_INFO_VERSION: u32 = 1;

/// Maximum number of normalized physical memory regions passed from bootloader to kernel.
pub const MAX_MEMORY_REGIONS: usize = 512;

/// Pixel layout of the UEFI Graphics Output Protocol (GOP) linear framebuffer.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 32-bit pixels: Blue, Green, Red, Reserved (most common on x86_64 UEFI).
    Bgr = 0,
    /// 32-bit pixels: Red, Green, Blue, Reserved.
    Rgb = 1,
    /// Custom bitmask or unknown format.
    Bitmask = 2,
}

/// Information about the active linear framebuffer configured by the UEFI bootloader.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    /// Physical base address of the linear framebuffer.
    pub base_address: u64,
    /// Total size of the framebuffer in bytes.
    pub buffer_size: usize,
    /// Horizontal resolution in pixels.
    pub width: usize,
    /// Vertical resolution in pixels.
    pub height: usize,
    /// Number of pixels per scanline (stride).
    pub pixels_per_scanline: usize,
    /// Color channel ordering.
    pub format: PixelFormat,
}

/// Classification of physical RAM regions reported by UEFI firmware.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryRegionKind {
    /// Free usable conventional RAM available to the kernel Physical Memory Manager (PMM).
    UsableRam = 0,
    /// Physical memory currently holding the loaded Celebron Kernel image and stack.
    KernelAndModules = 1,
    /// Reclaimable UEFI Boot Services / Loader memory.
    BootloaderReclaimable = 2,
    /// ACPI tables (reclaimable after parsing).
    AcpiReclaimable = 3,
    /// ACPI NVS or firmware runtime services memory (must be preserved).
    FirmwareReserved = 4,
    /// MMIO or hardware-reserved address range.
    MmioOrReserved = 5,
}

/// A contiguous physical memory range in the normalized memory map.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MemoryRegion {
    /// Page-aligned starting physical address.
    pub physical_start: u64,
    /// Length of the region in bytes (multiple of 4096).
    pub size_bytes: u64,
    /// Classification of the region.
    pub kind: MemoryRegionKind,
    pub _reserved: u32,
}

impl MemoryRegion {
    pub const fn empty() -> Self {
        Self {
            physical_start: 0,
            size_bytes: 0,
            kind: MemoryRegionKind::MmioOrReserved,
            _reserved: 0,
        }
    }
}

/// The master hand-off structure populated by the Celebron UEFI Bootloader
/// and passed in `rdi` (System V AMD64 ABI first argument) to `kernel_main`.
#[repr(C)]
pub struct BootInfo {
    /// Must equal [`BOOT_INFO_MAGIC`].
    pub magic: u64,
    /// Must equal [`BOOT_INFO_VERSION`].
    pub version: u32,
    /// Whether a valid GOP framebuffer is attached.
    pub has_framebuffer: u32,
    /// Linear framebuffer descriptor.
    pub framebuffer: FramebufferInfo,
    /// Physical address of the ACPI RSDP (Root System Description Pointer), or 0 if not found.
    pub acpi_rsdp_address: u64,
    /// Total usable conventional RAM detected (in bytes).
    pub total_usable_ram_bytes: u64,
    /// Number of valid entries in `memory_regions`.
    pub memory_region_count: usize,
    /// Static table of physical memory regions so no heap pointer indirection is required.
    pub memory_regions: [MemoryRegion; MAX_MEMORY_REGIONS],
}

impl BootInfo {
    pub const fn new() -> Self {
        Self {
            magic: BOOT_INFO_MAGIC,
            version: BOOT_INFO_VERSION,
            has_framebuffer: 0,
            framebuffer: FramebufferInfo {
                base_address: 0,
                buffer_size: 0,
                width: 0,
                height: 0,
                pixels_per_scanline: 0,
                format: PixelFormat::Bgr,
            },
            acpi_rsdp_address: 0,
            total_usable_ram_bytes: 0,
            memory_region_count: 0,
            memory_regions: [MemoryRegion::empty(); MAX_MEMORY_REGIONS],
        }
    }
}

/// Function pointer signature for the Celebron Microkernel entry point (`sysv64` calling convention).
pub type KernelEntryFn = extern "sysv64" fn(&'static BootInfo) -> !;
