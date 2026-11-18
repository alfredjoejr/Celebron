//! Celebron OS SMP (Symmetric Multiprocessing) & ACPI MADT Topology Discovery.
//! Parses ACPI 2.0/1.0 RSDP -> XSDT/RSDT -> MADT (`"APIC"`) tables to enumerate
//! logical CPU cores (Local APIC / x2APIC IDs), the Bootstrap Processor (BSP),
//! and the I/O APIC physical address.

use core::arch::asm;

pub const MAX_CPU_CORES: usize = 64;
const FOUR_GIB: u64 = 4 * 1024 * 1024 * 1024;

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct RsdpDescriptor {
    signature: [u8; 8],
    checksum: u8,
    oem_id: [u8; 6],
    revision: u8,
    rsdt_address: u32,
    length: u32,
    xsdt_address: u64,
    extended_checksum: u8,
    reserved: [u8; 3],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct AcpiSdtHeader {
    signature: [u8; 4],
    length: u32,
    revision: u8,
    checksum: u8,
    oem_id: [u8; 6],
    oem_table_id: [u8; 8],
    oem_revision: u32,
    creator_id: u32,
    creator_revision: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct SmpInfo {
    /// Number of enabled logical CPU cores discovered in ACPI MADT.
    pub detected_cores: usize,
    /// Local APIC ID of the current Bootstrap Processor (BSP).
    pub bsp_apic_id: u32,
    /// Physical base address of the primary I/O APIC discovered in MADT (default `0xFEC0_0000`).
    pub ioapic_phys_addr: u64,
    /// Whether ACPI MADT (`"APIC"`) was parsed successfully.
    pub madt_verified: bool,
    /// Enabled processor APIC IDs.
    pub core_apic_ids: [u32; MAX_CPU_CORES],
}

/// Query the current CPU's initial Local APIC ID via `CPUID` Leaf 1 (`EBX[31:24]`).
fn read_bsp_apic_id() -> u32 {
    let ebx_val: u32;
    unsafe {
        asm!(
            "push rbx",
            "mov eax, 1",
            "cpuid",
            "mov {0:e}, ebx",
            "pop rbx",
            out(reg) ebx_val,
            out("eax") _,
            out("ecx") _,
            out("edx") _,
            options(nomem, preserves_flags)
        );
    }
    ebx_val >> 24
}

/// Parse an ACPI MADT (`"APIC"`) table at `madt_phys` and populate `SmpInfo`.
unsafe fn parse_madt(madt_phys: u64, info: &mut SmpInfo) {
    if madt_phys == 0 || madt_phys + 44 >= FOUR_GIB {
        return;
    }

    let hdr = core::ptr::read_unaligned(madt_phys as *const AcpiSdtHeader);
    let total_len = hdr.length as usize;
    if total_len < 44 || madt_phys + (total_len as u64) >= FOUR_GIB {
        return;
    }

    info.madt_verified = true;

    // MADT entries begin at byte offset 44 (36-byte SDT header + 4-byte LAPIC addr + 4-byte flags)
    let base_ptr = madt_phys as *const u8;
    let mut offset = 44usize;
    let mut core_count = 0usize;

    while offset + 2 <= total_len {
        let entry_type = *base_ptr.add(offset);
        let entry_len = *base_ptr.add(offset + 1) as usize;
        if entry_len < 2 || offset + entry_len > total_len {
            break;
        }

        match entry_type {
            // Type 0: Processor Local APIC
            0 if entry_len >= 8 => {
                let apic_id = *base_ptr.add(offset + 3) as u32;
                let flags = core::ptr::read_unaligned(base_ptr.add(offset + 4) as *const u32);
                // Bit 0 = Processor Enabled, Bit 1 = Online Capable
                if (flags & 0x3) != 0 && core_count < MAX_CPU_CORES {
                    info.core_apic_ids[core_count] = apic_id;
                    core_count += 1;
                }
            }
            // Type 1: I/O APIC
            1 if entry_len >= 12 => {
                let ioapic_addr =
                    core::ptr::read_unaligned(base_ptr.add(offset + 4) as *const u32) as u64;
                if ioapic_addr != 0 {
                    info.ioapic_phys_addr = ioapic_addr;
                }
            }
            // Type 9: Processor Local x2APIC
            9 if entry_len >= 16 => {
                let x2apic_id =
                    core::ptr::read_unaligned(base_ptr.add(offset + 4) as *const u32);
                let flags = core::ptr::read_unaligned(base_ptr.add(offset + 8) as *const u32);
                if (flags & 0x3) != 0 && core_count < MAX_CPU_CORES {
                    info.core_apic_ids[core_count] = x2apic_id;
                    core_count += 1;
                }
            }
            _ => {}
        }

        offset += entry_len;
    }

    if core_count > 0 {
        info.detected_cores = core_count;
    }
}

/// Discover SMP processor topology and I/O APIC configuration from the ACPI RSDP.
pub fn init(acpi_rsdp_phys: u64) -> SmpInfo {
    let bsp_apic_id = read_bsp_apic_id();
    let mut info = SmpInfo {
        detected_cores: 1,
        bsp_apic_id,
        ioapic_phys_addr: 0xFEC0_0000,
        madt_verified: false,
        core_apic_ids: [0; MAX_CPU_CORES],
    };
    info.core_apic_ids[0] = bsp_apic_id;

    if acpi_rsdp_phys == 0 || acpi_rsdp_phys + 36 >= FOUR_GIB {
        return info;
    }

    unsafe {
        let rsdp = core::ptr::read_unaligned(acpi_rsdp_phys as *const RsdpDescriptor);
        if &rsdp.signature != b"RSD PTR " {
            return info;
        }

        let hdr_size = core::mem::size_of::<AcpiSdtHeader>();

        // Prefer 64-bit XSDT (ACPI 2.0+), fallback to 32-bit RSDT
        if rsdp.revision >= 2 && rsdp.xsdt_address != 0 && rsdp.xsdt_address + (hdr_size as u64) < FOUR_GIB {
            let xsdt_phys = rsdp.xsdt_address;
            let xsdt_hdr = core::ptr::read_unaligned(xsdt_phys as *const AcpiSdtHeader);
            if &xsdt_hdr.signature == b"XSDT" && (xsdt_hdr.length as usize) >= hdr_size {
                let entries_bytes = (xsdt_hdr.length as usize) - hdr_size;
                let num_entries = entries_bytes / 8;
                let entries_ptr = (xsdt_phys as *const u8).add(hdr_size) as *const u64;

                for i in 0..num_entries {
                    let table_phys = core::ptr::read_unaligned(entries_ptr.add(i));
                    if table_phys != 0 && table_phys + (hdr_size as u64) < FOUR_GIB {
                        let thdr = core::ptr::read_unaligned(table_phys as *const AcpiSdtHeader);
                        if &thdr.signature == b"APIC" {
                            parse_madt(table_phys, &mut info);
                            return info;
                        }
                    }
                }
            }
        }

        if rsdp.rsdt_address != 0 && (rsdp.rsdt_address as u64) + (hdr_size as u64) < FOUR_GIB {
            let rsdt_phys = rsdp.rsdt_address as u64;
            let rsdt_hdr = core::ptr::read_unaligned(rsdt_phys as *const AcpiSdtHeader);
            if &rsdt_hdr.signature == b"RSDT" && (rsdt_hdr.length as usize) >= hdr_size {
                let entries_bytes = (rsdt_hdr.length as usize) - hdr_size;
                let num_entries = entries_bytes / 4;
                let entries_ptr = (rsdt_phys as *const u8).add(hdr_size) as *const u32;

                for i in 0..num_entries {
                    let table_phys = core::ptr::read_unaligned(entries_ptr.add(i)) as u64;
                    if table_phys != 0 && table_phys + (hdr_size as u64) < FOUR_GIB {
                        let thdr = core::ptr::read_unaligned(table_phys as *const AcpiSdtHeader);
                        if &thdr.signature == b"APIC" {
                            parse_madt(table_phys, &mut info);
                            return info;
                        }
                    }
                }
            }
        }
    }

    info
}
