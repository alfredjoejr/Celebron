//! Celebron OS Physical Memory Manager (PMM).
//! Tracks 4 KiB physical page frames using a static bitmap in `.bss` and protects
//! low memory, the kernel ELF image, BootInfo, and firmware regions from allocation.

use celebron_boot_info::{BootInfo, MemoryRegionKind};
use core::ptr::addr_of_mut;

pub const PAGE_SIZE: u64 = 4096;

/// Minimum physical address available for dynamic PMM allocation (32 MiB).
/// Leaves 0..32 MiB reserved for BIOS/AP bringup, the kernel image at 16 MiB,
/// `.bss` (including the PMM bitmap itself), BootInfo, and boot stack.
pub const MIN_ALLOCATABLE_PHYS_ADDR: u64 = 0x0200_0000;

/// Maximum physical address tracked by the static bitmap (16 GiB).
pub const MAX_PHYS_RAM_BYTES: u64 = 16 * 1024 * 1024 * 1024;

const TOTAL_TRACKED_FRAMES: usize = (MAX_PHYS_RAM_BYTES / PAGE_SIZE) as usize;
const BITMAP_WORDS: usize = TOTAL_TRACKED_FRAMES / 64;

/// Global physical frame bitmap (`1` = used/reserved, `0` = free).
/// Initialized to all `1`s so unmapped or firmware regions are never allocated.
static mut PMM_BITMAP: [u64; BITMAP_WORDS] = [u64::MAX; BITMAP_WORDS];

#[inline(always)]
unsafe fn read_word(word_idx: usize) -> u64 {
    let base = addr_of_mut!(PMM_BITMAP) as *const u64;
    base.add(word_idx).read()
}

#[inline(always)]
unsafe fn write_word(word_idx: usize, value: u64) {
    let base = addr_of_mut!(PMM_BITMAP) as *mut u64;
    base.add(word_idx).write(value);
}

#[derive(Debug, Clone, Copy)]
pub struct PmmStats {
    pub free_frames: usize,
    pub used_frames: usize,
    pub total_usable_frames: usize,
    pub highest_phys_addr: u64,
}

pub struct PhysicalMemoryManager {
    free_frames: usize,
    used_frames: usize,
    total_usable_frames: usize,
    highest_phys_addr: u64,
    search_start_word: usize,
}

impl PhysicalMemoryManager {
    /// Parse the normalized UEFI memory map in `BootInfo` and populate the PMM frame bitmap.
    pub fn init(boot_info: &BootInfo) -> Self {
        // Safety: Single-threaded Ring-0 boot initialization before any concurrent access.
        unsafe {
            core::ptr::write_bytes(addr_of_mut!(PMM_BITMAP) as *mut u8, 0xFF, BITMAP_WORDS * 8);
        }

        let mut free_frames = 0usize;
        let mut total_usable_frames = 0usize;
        let mut highest_phys_addr = 0u64;
        let mut search_start_word = BITMAP_WORDS;

        let region_count = boot_info
            .memory_region_count
            .min(boot_info.memory_regions.len());

        for i in 0..region_count {
            let region = &boot_info.memory_regions[i];
            if region.kind != MemoryRegionKind::UsableRam {
                continue;
            }

            let raw_start = region.physical_start;
            let raw_end = raw_start.saturating_add(region.size_bytes);

            let aligned_start = (raw_start.saturating_add(PAGE_SIZE - 1)) & !(PAGE_SIZE - 1);
            let start = aligned_start.max(MIN_ALLOCATABLE_PHYS_ADDR);
            let aligned_end = raw_end & !(PAGE_SIZE - 1);
            let end = aligned_end.min(MAX_PHYS_RAM_BYTES);

            if start >= end {
                continue;
            }

            if end > highest_phys_addr {
                highest_phys_addr = end;
            }

            let mut addr = start;
            while addr < end {
                let frame_idx = (addr / PAGE_SIZE) as usize;
                let word_idx = frame_idx / 64;
                let bit_idx = frame_idx % 64;

                unsafe {
                    let mut word = read_word(word_idx);
                    if (word & (1u64 << bit_idx)) != 0 {
                        word &= !(1u64 << bit_idx);
                        write_word(word_idx, word);
                        free_frames += 1;
                        total_usable_frames += 1;
                        if word_idx < search_start_word {
                            search_start_word = word_idx;
                        }
                    }
                }

                addr += PAGE_SIZE;
            }
        }

        Self {
            free_frames,
            used_frames: 0,
            total_usable_frames,
            highest_phys_addr,
            search_start_word,
        }
    }

    /// Allocate one 4 KiB physical frame, zero its contents, and return its physical address.
    pub fn alloc_frame(&mut self) -> Option<u64> {
        if self.free_frames == 0 {
            return None;
        }

        for word_idx in self.search_start_word..BITMAP_WORDS {
            let mut word = unsafe { read_word(word_idx) };
            if word != u64::MAX {
                let bit_idx = (!word).trailing_zeros() as usize;
                word |= 1u64 << bit_idx;
                unsafe { write_word(word_idx, word) };

                self.search_start_word = word_idx;
                self.free_frames -= 1;
                self.used_frames += 1;

                let phys_addr = ((word_idx * 64 + bit_idx) as u64) * PAGE_SIZE;
                unsafe {
                    core::ptr::write_bytes(phys_addr as *mut u8, 0, PAGE_SIZE as usize);
                }
                return Some(phys_addr);
            }
        }

        None
    }

    /// Return a previously allocated 4 KiB physical frame to the free pool.
    #[allow(dead_code)]
    pub fn free_frame(&mut self, phys_addr: u64) {
        if (phys_addr & (PAGE_SIZE - 1)) != 0
            || phys_addr < MIN_ALLOCATABLE_PHYS_ADDR
            || phys_addr >= MAX_PHYS_RAM_BYTES
        {
            return;
        }

        let frame_idx = (phys_addr / PAGE_SIZE) as usize;
        let word_idx = frame_idx / 64;
        let bit_idx = frame_idx % 64;

        unsafe {
            let mut word = read_word(word_idx);
            if (word & (1u64 << bit_idx)) == 0 {
                // Already free; guard against double free.
                return;
            }
            word &= !(1u64 << bit_idx);
            write_word(word_idx, word);
        }

        self.free_frames += 1;
        self.used_frames = self.used_frames.saturating_sub(1);
        if word_idx < self.search_start_word {
            self.search_start_word = word_idx;
        }
    }

    pub fn stats(&self) -> PmmStats {
        PmmStats {
            free_frames: self.free_frames,
            used_frames: self.used_frames,
            total_usable_frames: self.total_usable_frames,
            highest_phys_addr: self.highest_phys_addr,
        }
    }
}
