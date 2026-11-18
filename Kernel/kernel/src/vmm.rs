//! Celebron OS Virtual Memory Manager (VMM).
//! Builds 4-level x86_64 page tables (PML4 -> PDPT -> PD -> PT), establishes both
//! a 4 GiB identity map + Higher-Half Direct Physical Map + Higher-Half Kernel Heap,
//! and atomically switches CR3 to Celebron's own page tables.

use crate::pmm::PhysicalMemoryManager;
use celebron_boot_info::BootInfo;
use core::arch::asm;

pub const PAGE_PRESENT: u64 = 1 << 0;
pub const PAGE_WRITABLE: u64 = 1 << 1;
#[allow(dead_code)]
pub const PAGE_USER: u64 = 1 << 2;
#[allow(dead_code)]
pub const PAGE_WRITE_THROUGH: u64 = 1 << 3;
#[allow(dead_code)]
pub const PAGE_CACHE_DISABLE: u64 = 1 << 4;
pub const PAGE_HUGE_2M: u64 = 1 << 7;

pub const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
const HUGE_2M_SIZE: u64 = 2 * 1024 * 1024;
const HUGE_2M_ADDR_MASK: u64 = 0x000F_FFFF_FFE0_0000;

/// Higher-Half Direct Physical Map base (`PML4[256]`).
pub const HIGHER_HALF_PHYS_OFFSET: u64 = 0xFFFF_8000_0000_0000;

/// Higher-Half Kernel Heap virtual base (`PML4[384]`).
pub const KERNEL_HEAP_VIRT_START: u64 = 0xFFFF_C000_0000_0000;

/// Initial kernel heap mapping: 1024 x 4 KiB pages = 4 MiB.
pub const KERNEL_HEAP_PAGES: usize = 1024;
pub const KERNEL_HEAP_SIZE_BYTES: usize = KERNEL_HEAP_PAGES * 4096;

#[repr(C, align(4096))]
pub struct PageTable {
    pub entries: [u64; 512],
}

pub struct VirtualMemoryManager {
    pub pml4_phys: u64,
    pub page_tables_allocated: usize,
    pub mapped_heap_pages: usize,
}

impl VirtualMemoryManager {
    /// Fetch an existing child page table or allocate a fresh zeroed 4 KiB frame from PMM.
    unsafe fn get_or_alloc_next_table(
        parent: &mut PageTable,
        index: usize,
        pmm: &mut PhysicalMemoryManager,
        page_tables_allocated: &mut usize,
    ) -> &'static mut PageTable {
        if (parent.entries[index] & PAGE_PRESENT) == 0 {
            let frame_phys = pmm
                .alloc_frame()
                .expect("Out of physical memory while allocating page table");
            // `alloc_frame` guarantees the 4 KiB frame is already zeroed.
            parent.entries[index] = frame_phys | PAGE_PRESENT | PAGE_WRITABLE;
            *page_tables_allocated += 1;
        }
        let child_phys = parent.entries[index] & ADDR_MASK;
        &mut *(child_phys as *mut PageTable)
    }

    /// Map a 2 MiB huge page at `virt_addr` -> `phys_addr` (both 2 MiB aligned).
    pub unsafe fn map_2m(
        &mut self,
        virt_addr: u64,
        phys_addr: u64,
        flags: u64,
        pmm: &mut PhysicalMemoryManager,
    ) {
        let pml4_idx = ((virt_addr >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((virt_addr >> 30) & 0x1FF) as usize;
        let pd_idx = ((virt_addr >> 21) & 0x1FF) as usize;

        let pml4 = &mut *(self.pml4_phys as *mut PageTable);
        let pdpt = Self::get_or_alloc_next_table(
            pml4,
            pml4_idx,
            pmm,
            &mut self.page_tables_allocated,
        );
        let pd = Self::get_or_alloc_next_table(
            pdpt,
            pdpt_idx,
            pmm,
            &mut self.page_tables_allocated,
        );

        pd.entries[pd_idx] =
            (phys_addr & HUGE_2M_ADDR_MASK) | flags | PAGE_PRESENT | PAGE_HUGE_2M;
    }

    /// Map a 4 KiB page at `virt_addr` -> `phys_addr` (both 4 KiB aligned).
    pub unsafe fn map_4k(
        &mut self,
        virt_addr: u64,
        phys_addr: u64,
        flags: u64,
        pmm: &mut PhysicalMemoryManager,
    ) {
        let pml4_idx = ((virt_addr >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((virt_addr >> 30) & 0x1FF) as usize;
        let pd_idx = ((virt_addr >> 21) & 0x1FF) as usize;
        let pt_idx = ((virt_addr >> 12) & 0x1FF) as usize;

        let pml4 = &mut *(self.pml4_phys as *mut PageTable);
        let pdpt = Self::get_or_alloc_next_table(
            pml4,
            pml4_idx,
            pmm,
            &mut self.page_tables_allocated,
        );
        let pd = Self::get_or_alloc_next_table(
            pdpt,
            pdpt_idx,
            pmm,
            &mut self.page_tables_allocated,
        );
        let pt = Self::get_or_alloc_next_table(
            pd,
            pd_idx,
            pmm,
            &mut self.page_tables_allocated,
        );

        pt.entries[pt_idx] = (phys_addr & ADDR_MASK) | flags | PAGE_PRESENT;
    }

    /// Construct Celebron's 4-level page tables and load `CR3`.
    pub fn init(pmm: &mut PhysicalMemoryManager, boot_info: &BootInfo) -> Self {
        let pml4_phys = pmm
            .alloc_frame()
            .expect("Out of physical memory allocating root PML4");

        let mut vmm = Self {
            pml4_phys,
            page_tables_allocated: 1, // Counts the root PML4 itself
            mapped_heap_pages: 0,
        };

        unsafe {
            // 1. Identity-map 0..4 GiB (preserves kernel @ 16 MiB, BootInfo, boot stack,
            //    COM1, ACPI, and GOP framebuffer) + Higher-Half Direct Physical Map @ 0xFFFF_8000_0000_0000.
            let four_gib: u64 = 4 * 1024 * 1024 * 1024;
            let mut phys: u64 = 0;
            while phys < four_gib {
                vmm.map_2m(phys, phys, PAGE_WRITABLE, pmm);
                vmm.map_2m(HIGHER_HALF_PHYS_OFFSET + phys, phys, PAGE_WRITABLE, pmm);
                phys += HUGE_2M_SIZE;
            }

            // 2. If GOP framebuffer resides above 4 GiB, identity-map its 2 MiB pages explicitly.
            if boot_info.has_framebuffer != 0 {
                let fb = &boot_info.framebuffer;
                if fb.base_address >= four_gib && fb.buffer_size > 0 {
                    let fb_start = fb.base_address & !(HUGE_2M_SIZE - 1);
                    let fb_end = (fb.base_address.saturating_add(fb.buffer_size as u64)
                        + HUGE_2M_SIZE
                        - 1)
                        & !(HUGE_2M_SIZE - 1);
                    let mut addr = fb_start;
                    while addr < fb_end {
                        vmm.map_2m(addr, addr, PAGE_WRITABLE | PAGE_WRITE_THROUGH, pmm);
                        addr += HUGE_2M_SIZE;
                    }
                }
            }

            // 3. Allocate 1024 x 4 KiB frames (4 MiB) and map them at KERNEL_HEAP_VIRT_START.
            for page_idx in 0..KERNEL_HEAP_PAGES {
                let frame_phys = pmm
                    .alloc_frame()
                    .expect("Out of physical memory mapping kernel heap");
                let virt = KERNEL_HEAP_VIRT_START + ((page_idx as u64) * 4096);
                vmm.map_4k(virt, frame_phys, PAGE_WRITABLE, pmm);
                vmm.mapped_heap_pages += 1;
            }

            // 4. Switch CR3 to Celebron's newly built PML4.
            asm!("mov cr3, {}", in(reg) vmm.pml4_phys, options(nostack, preserves_flags));
        }

        vmm
    }
}
