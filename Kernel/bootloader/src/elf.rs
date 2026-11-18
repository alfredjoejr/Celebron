//! Proprietary ELF64 Executable Loader for the Celebron UEFI Bootloader.
//! Parses a 64-bit `x86_64` ELF executable (`ET_EXEC` or `ET_DYN`) and loads all `PT_LOAD`
//! segments into physical memory allocated via UEFI Boot Services.

use crate::uefi_raw::{
    EfiBootServices, ALLOCATE_ADDRESS, EFI_LOADER_CODE,
};

const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Elf64Ehdr {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_flags: u32,
    e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
    e_shentsize: u16,
    e_shnum: u16,
    e_shstrndx: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Elf64Phdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct LoadedKernelInfo {
    /// Entry point address (`e_entry`) to jump to after `ExitBootServices`.
    pub entry_point: u64,
    /// Lowest physical address loaded by any `PT_LOAD` segment.
    pub lowest_phys_addr: u64,
    /// Highest physical address (page-aligned end) occupied by the kernel.
    pub highest_phys_addr: u64,
    /// Number of loadable segments processed.
    pub segment_count: usize,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub enum ElfLoadError {
    BufferTooSmall,
    InvalidMagic,
    Not64BitLittleEndian,
    UnsupportedArchitecture,
    InvalidProgramHeaderTable,
    SegmentOutOfBounds,
    PageAllocationFailed(u64),
}

/// Validates and loads an ELF64 kernel image into physical memory using UEFI Boot Services.
pub unsafe fn load_elf64_kernel(
    boot_services: &EfiBootServices,
    elf_bytes: &[u8],
) -> Result<LoadedKernelInfo, ElfLoadError> {
    if elf_bytes.len() < core::mem::size_of::<Elf64Ehdr>() {
        return Err(ElfLoadError::BufferTooSmall);
    }

    let ehdr = core::ptr::read_unaligned(elf_bytes.as_ptr() as *const Elf64Ehdr);

    if ehdr.e_ident[0..4] != ELF_MAGIC {
        return Err(ElfLoadError::InvalidMagic);
    }
    if ehdr.e_ident[4] != ELFCLASS64 || ehdr.e_ident[5] != ELFDATA2LSB {
        return Err(ElfLoadError::Not64BitLittleEndian);
    }
    if ehdr.e_machine != EM_X86_64 {
        return Err(ElfLoadError::UnsupportedArchitecture);
    }

    let phoff = ehdr.e_phoff as usize;
    let phentsize = ehdr.e_phentsize as usize;
    let phnum = ehdr.e_phnum as usize;

    if phentsize < core::mem::size_of::<Elf64Phdr>()
        || phoff.saturating_add(phentsize.saturating_mul(phnum)) > elf_bytes.len()
    {
        return Err(ElfLoadError::InvalidProgramHeaderTable);
    }

    let mut lowest_phys = u64::MAX;
    let mut highest_phys = 0u64;
    let mut segment_count = 0usize;

    for i in 0..phnum {
        let ph_ptr = elf_bytes.as_ptr().add(phoff + i * phentsize) as *const Elf64Phdr;
        let phdr = core::ptr::read_unaligned(ph_ptr);

        if phdr.p_type != PT_LOAD || phdr.p_memsz == 0 {
            continue;
        }

        let file_offset = phdr.p_offset as usize;
        let file_size = phdr.p_filesz as usize;
        let mem_size = phdr.p_memsz as usize;

        if file_size > mem_size || file_offset.saturating_add(file_size) > elf_bytes.len() {
            return Err(ElfLoadError::SegmentOutOfBounds);
        }

        let target_phys = if phdr.p_paddr != 0 {
            phdr.p_paddr
        } else {
            phdr.p_vaddr
        };

        let page_start = target_phys & !0xFFFu64;
        let page_end = (target_phys + phdr.p_memsz + 0xFFFu64) & !0xFFFu64;
        let num_pages = ((page_end - page_start) / 4096) as usize;

        // Allocate the physical pages in UEFI so the firmware marks them as LoaderCode.
        // (If pages overlap between segments, allocation at the exact address may already be owned,
        // so we allocate page-by-page to handle overlapping 4 KiB page boundaries gracefully.)
        let mut page_addr = page_start;
        for _ in 0..num_pages {
            let mut addr = page_addr;
            let _status = (boot_services.allocate_pages)(
                ALLOCATE_ADDRESS,
                EFI_LOADER_CODE,
                1,
                &mut addr,
            );
            page_addr += 4096;
        }

        // Zero the entire memory segment range (covers .bss automatically)
        let dest_ptr = target_phys as *mut u8;
        core::ptr::write_bytes(dest_ptr, 0, mem_size);

        // Copy initialized segment bytes from the ELF image
        if file_size > 0 {
            let src_ptr = elf_bytes.as_ptr().add(file_offset);
            core::ptr::copy_nonoverlapping(src_ptr, dest_ptr, file_size);
        }

        if page_start < lowest_phys {
            lowest_phys = page_start;
        }
        if page_end > highest_phys {
            highest_phys = page_end;
        }
        segment_count += 1;
    }

    if segment_count == 0 {
        return Err(ElfLoadError::InvalidProgramHeaderTable);
    }

    Ok(LoadedKernelInfo {
        entry_point: ehdr.e_entry,
        lowest_phys_addr: lowest_phys,
        highest_phys_addr: highest_phys,
        segment_count,
    })
}
