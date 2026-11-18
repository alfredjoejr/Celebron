//! Celebron OS ACPI Table Parser (FADT, MCFG, DSDT `_S5_`).
//! Extracts hardware power management registers (`PM1a_CNT_BLK`, `RESET_REG`, `SLEEP_CONTROL_REG`),
//! DSDT `_S5_` sleep type values for clean ACPI shutdown/reboot, and PCIe ECAM base addresses
//! from the `MCFG` table.

use core::ptr::read_unaligned;

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

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct GenericAddressStructure {
    /// 0 = System Memory (MMIO), 1 = System I/O Port, 2 = PCI Config Space
    pub address_space_id: u8,
    pub register_bit_width: u8,
    pub register_bit_offset: u8,
    pub access_size: u8,
    pub address: u64,
}

impl GenericAddressStructure {
    pub const fn empty() -> Self {
        Self {
            address_space_id: 0,
            register_bit_width: 0,
            register_bit_offset: 0,
            access_size: 0,
            address: 0,
        }
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct McfgAllocationEntry {
    base_address: u64,
    pci_segment_group: u16,
    start_bus: u8,
    end_bus: u8,
    _reserved: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct AcpiInfo {
    pub tables_found: usize,
    pub fadt_found: bool,
    pub mcfg_found: bool,
    pub s5_found: bool,
    pub hw_reduced: bool,
    pub reset_reg_supported: bool,
    pub sci_interrupt: u16,
    pub smi_cmd_port: u32,
    pub acpi_enable_cmd: u8,
    pub pm1a_cnt_blk: u32,
    pub pm1b_cnt_blk: u32,
    pub slp_typa: u16,
    pub slp_typb: u16,
    pub reset_reg: GenericAddressStructure,
    pub reset_value: u8,
    pub sleep_control_reg: GenericAddressStructure,
    pub pcie_ecam_base: u64,
    pub pcie_start_bus: u8,
    pub pcie_end_bus: u8,
}

impl AcpiInfo {
    pub const fn empty() -> Self {
        Self {
            tables_found: 0,
            fadt_found: false,
            mcfg_found: false,
            s5_found: false,
            hw_reduced: false,
            reset_reg_supported: false,
            sci_interrupt: 0,
            smi_cmd_port: 0,
            acpi_enable_cmd: 0,
            pm1a_cnt_blk: 0,
            pm1b_cnt_blk: 0,
            slp_typa: 5,
            slp_typb: 0,
            reset_reg: GenericAddressStructure::empty(),
            reset_value: 0,
            sleep_control_reg: GenericAddressStructure::empty(),
            pcie_ecam_base: 0,
            pcie_start_bus: 0,
            pcie_end_bus: 0,
        }
    }
}

/// Parse an AML integer value (`ZeroOp`, `OneOp`, `BytePrefix`, or literal byte) for `_S5_`.
fn read_aml_slp_typ(bytes: &[u8], idx: &mut usize) -> Option<u16> {
    if *idx >= bytes.len() {
        return None;
    }
    let op = bytes[*idx];
    *idx += 1;
    match op {
        0x00 => Some(0), // ZeroOp
        0x01 => Some(1), // OneOp
        0x0A => {
            // BytePrefix
            if *idx < bytes.len() {
                let val = bytes[*idx] as u16;
                *idx += 1;
                Some(val)
            } else {
                None
            }
        }
        0x0B => {
            // WordPrefix
            if *idx + 1 < bytes.len() {
                let val = u16::from_le_bytes([bytes[*idx], bytes[*idx + 1]]);
                *idx += 2;
                Some(val)
            } else {
                None
            }
        }
        other => Some(other as u16),
    }
}

/// Scan the DSDT AML table for the `_S5_` package (`SLP_TYPa`, `SLP_TYPb`).
unsafe fn parse_dsdt_s5(dsdt_addr: u64) -> Option<(u16, u16)> {
    if dsdt_addr == 0 {
        return None;
    }
    let hdr = read_unaligned(dsdt_addr as *const AcpiSdtHeader);
    if &hdr.signature != b"DSDT" || hdr.length < 36 || hdr.length > 4 * 1024 * 1024 {
        return None;
    }

    let total_len = hdr.length as usize;
    let bytes = core::slice::from_raw_parts(dsdt_addr as *const u8, total_len);

    // Search for "_S5_" followed by PackageOp (0x12)
    let mut i = 36;
    while i + 8 < bytes.len() {
        if &bytes[i..i + 4] == b"_S5_" {
            // Check valid NameOp (0x08) or root/scope prefix before "_S5_"
            let prev_ok = i > 36 && (bytes[i - 1] == 0x08 || bytes[i - 1] == b'\\' || bytes[i - 1] == b'^');
            if prev_ok && bytes[i + 4] == 0x12 {
                let mut pos = i + 5;
                if pos >= bytes.len() {
                    break;
                }
                // Decode AML PkgLength (top 2 bits = number of additional length bytes)
                let pkg_lead = bytes[pos];
                let extra_len_bytes = ((pkg_lead >> 6) & 0x03) as usize;
                pos += 1 + extra_len_bytes;

                // Skip NumElements byte
                if pos >= bytes.len() {
                    break;
                }
                pos += 1;

                let slp_typa = read_aml_slp_typ(bytes, &mut pos)?;
                let slp_typb = read_aml_slp_typ(bytes, &mut pos).unwrap_or(0);
                return Some((slp_typa, slp_typb));
            }
        }
        i += 1;
    }
    None
}

unsafe fn parse_fadt(fadt_addr: u64, info: &mut AcpiInfo) {
    let hdr = read_unaligned(fadt_addr as *const AcpiSdtHeader);
    let len = hdr.length as usize;
    if len < 116 {
        return;
    }

    info.fadt_found = true;
    let base = fadt_addr as *const u8;

    let dsdt32 = read_unaligned(base.add(40) as *const u32) as u64;
    info.sci_interrupt = read_unaligned(base.add(46) as *const u16);
    info.smi_cmd_port = read_unaligned(base.add(48) as *const u32);
    info.acpi_enable_cmd = read_unaligned(base.add(52));
    info.pm1a_cnt_blk = read_unaligned(base.add(64) as *const u32);
    info.pm1b_cnt_blk = read_unaligned(base.add(68) as *const u32);

    let flags = read_unaligned(base.add(112) as *const u32);
    info.reset_reg_supported = (flags & (1 << 10)) != 0;
    info.hw_reduced = (flags & (1 << 20)) != 0;

    if len >= 129 {
        info.reset_reg = read_unaligned(base.add(116) as *const GenericAddressStructure);
        info.reset_value = read_unaligned(base.add(128));
    }

    let dsdt64 = if len >= 148 {
        read_unaligned(base.add(140) as *const u64)
    } else {
        0
    };

    if len >= 256 {
        info.sleep_control_reg = read_unaligned(base.add(244) as *const GenericAddressStructure);
    }

    let dsdt_addr = if dsdt64 != 0 { dsdt64 } else { dsdt32 };
    if let Some((typa, typb)) = parse_dsdt_s5(dsdt_addr) {
        info.slp_typa = typa;
        info.slp_typb = typb;
        info.s5_found = true;
    }
}

unsafe fn parse_mcfg(mcfg_addr: u64, info: &mut AcpiInfo) {
    let hdr = read_unaligned(mcfg_addr as *const AcpiSdtHeader);
    let len = hdr.length as usize;
    if len < 44 + core::mem::size_of::<McfgAllocationEntry>() {
        return;
    }

    let entry = read_unaligned((mcfg_addr + 44) as *const McfgAllocationEntry);
    if entry.base_address != 0 {
        info.mcfg_found = true;
        info.pcie_ecam_base = entry.base_address;
        info.pcie_start_bus = entry.start_bus;
        info.pcie_end_bus = entry.end_bus;
    }
}

/// Walk ACPI XSDT/RSDT from `rsdp_addr` and parse `FACP` (FADT), `DSDT` (`_S5_`), and `MCFG`.
pub fn init(rsdp_addr: u64) -> AcpiInfo {
    let mut info = AcpiInfo::empty();
    if rsdp_addr == 0 {
        return info;
    }

    unsafe {
        let rsdp = read_unaligned(rsdp_addr as *const RsdpDescriptor);
        if &rsdp.signature != b"RSD PTR " {
            return info;
        }

        let use_xsdt = rsdp.revision >= 2 && rsdp.xsdt_address != 0;
        let root_table_addr = if use_xsdt {
            rsdp.xsdt_address
        } else {
            rsdp.rsdt_address as u64
        };
        if root_table_addr == 0 {
            return info;
        }

        let root_hdr = read_unaligned(root_table_addr as *const AcpiSdtHeader);
        let hdr_size = core::mem::size_of::<AcpiSdtHeader>();
        if (root_hdr.length as usize) < hdr_size {
            return info;
        }

        let entry_size = if use_xsdt { 8usize } else { 4usize };
        let entry_count = (root_hdr.length as usize - hdr_size) / entry_size;
        let entries_base = root_table_addr + hdr_size as u64;

        info.tables_found = entry_count;

        for i in 0..entry_count {
            let table_addr = if use_xsdt {
                read_unaligned((entries_base + (i * 8) as u64) as *const u64)
            } else {
                read_unaligned((entries_base + (i * 4) as u64) as *const u32) as u64
            };
            if table_addr == 0 {
                continue;
            }
            let tbl_hdr = read_unaligned(table_addr as *const AcpiSdtHeader);
            match &tbl_hdr.signature {
                b"FACP" => parse_fadt(table_addr, &mut info),
                b"MCFG" => parse_mcfg(table_addr, &mut info),
                _ => {}
            }
        }
    }

    info
}
