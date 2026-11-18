//! Celebron OS PCIe Bus Enumeration & Hypervisor Platform Detection.
//! Enumerates PCIe devices via ACPI `MCFG` ECAM MMIO or Legacy PCI Config Space (`0xCF8`/`0xCFC`),
//! classifies storage/network/display controllers (NVMe, VirtIO-Blk, AHCI, xHCI),
//! and detects host hypervisor presence via `CPUID` Leaf `0x4000_0000`.

use crate::acpi::AcpiInfo;
use core::arch::asm;

pub const MAX_PCI_DEVICES: usize = 32;

const PCI_CONFIG_ADDR_PORT: u16 = 0x0CF8;
const PCI_CONFIG_DATA_PORT: u16 = 0x0CFC;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageKind {
    None,
    Nvme,
    VirtioBlk,
    Ahci,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct PciDeviceInfo {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class_code: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub bar0: u64,
    pub irq_line: u8,
    pub storage_kind: StorageKind,
}

impl PciDeviceInfo {
    pub const fn empty() -> Self {
        Self {
            bus: 0,
            device: 0,
            function: 0,
            vendor_id: 0xFFFF,
            device_id: 0xFFFF,
            class_code: 0,
            subclass: 0,
            prog_if: 0,
            bar0: 0,
            irq_line: 0,
            storage_kind: StorageKind::None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PcieEnumerationResult {
    pub hypervisor_present: bool,
    pub hypervisor_vendor: [u8; 12],
    pub is_hyperv: bool,
    pub used_ecam: bool,
    pub device_count: usize,
    pub storage_controllers: usize,
    pub devices: [PciDeviceInfo; MAX_PCI_DEVICES],
}

impl PcieEnumerationResult {
    pub fn hypervisor_vendor_str(&self) -> &str {
        if !self.hypervisor_present {
            return "BARE-METAL";
        }
        core::str::from_utf8(&self.hypervisor_vendor)
            .unwrap_or("HYPERVISOR")
            .trim_end_matches('\0')
    }
}

#[inline(always)]
unsafe fn outl(port: u16, val: u32) {
    asm!("out dx, eax", in("dx") port, in("eax") val, options(nomem, nostack, preserves_flags));
}

#[inline(always)]
unsafe fn inl(port: u16) -> u32 {
    let val: u32;
    asm!("in eax, dx", out("eax") val, in("dx") port, options(nomem, nostack, preserves_flags));
    val
}

fn cpuid(leaf: u32) -> (u32, u32, u32, u32) {
    let eax: u32;
    let ebx: u32;
    let ecx: u32;
    let edx: u32;
    unsafe {
        asm!(
            "push rbx",
            "cpuid",
            "mov {ebx_out:e}, ebx",
            "pop rbx",
            inout("eax") leaf => eax,
            ebx_out = out(reg) ebx,
            out("ecx") ecx,
            out("edx") edx,
            options(nomem, preserves_flags),
        );
    }
    (eax, ebx, ecx, edx)
}

fn detect_hypervisor() -> (bool, [u8; 12], bool) {
    let (_, _, ecx, _) = cpuid(1);
    if (ecx & (1 << 31)) == 0 {
        return (false, [0; 12], false);
    }
    let (_, ebx, ecx_hv, edx) = cpuid(0x4000_0000);
    let mut vendor = [0u8; 12];
    vendor[0..4].copy_from_slice(&ebx.to_le_bytes());
    vendor[4..8].copy_from_slice(&ecx_hv.to_le_bytes());
    vendor[8..12].copy_from_slice(&edx.to_le_bytes());
    let is_hyperv = &vendor == b"Microsoft Hv";
    (true, vendor, is_hyperv)
}

unsafe fn pci_read_u32_legacy(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    let addr: u32 = (1u32 << 31)
        | ((bus as u32) << 16)
        | (((dev as u32) & 0x1F) << 11)
        | (((func as u32) & 0x07) << 8)
        | ((offset as u32) & 0xFC);
    outl(PCI_CONFIG_ADDR_PORT, addr);
    inl(PCI_CONFIG_DATA_PORT)
}

unsafe fn pci_read_u32_ecam(ecam_base: u64, bus: u8, dev: u8, func: u8, offset: u16) -> u32 {
    let addr = ecam_base
        + ((bus as u64) << 20)
        + ((dev as u64) << 15)
        + ((func as u64) << 12)
        + ((offset as u64) & 0xFFC);
    core::ptr::read_volatile(addr as *const u32)
}

fn classify_storage(vendor_id: u16, device_id: u16, class_code: u8, subclass: u8, prog_if: u8) -> StorageKind {
    if class_code == 0x01 && subclass == 0x08 && prog_if == 0x02 {
        return StorageKind::Nvme;
    }
    if vendor_id == 0x1AF4 && (device_id == 0x1001 || device_id == 0x1042) {
        return StorageKind::VirtioBlk;
    }
    if class_code == 0x01 && subclass == 0x06 && prog_if == 0x01 {
        return StorageKind::Ahci;
    }
    StorageKind::None
}

/// Enumerate PCIe / PCI bus devices and detect host hypervisor platform.
pub fn enumerate(acpi: &AcpiInfo) -> PcieEnumerationResult {
    let (hypervisor_present, hypervisor_vendor, is_hyperv) = detect_hypervisor();

    // Use ECAM only if MCFG was found and resides within our 4 GiB identity-mapped window
    let use_ecam = acpi.mcfg_found && acpi.pcie_ecam_base != 0 && acpi.pcie_ecam_base < 0x1_0000_0000;

    let mut result = PcieEnumerationResult {
        hypervisor_present,
        hypervisor_vendor,
        is_hyperv,
        used_ecam: use_ecam,
        device_count: 0,
        storage_controllers: 0,
        devices: [PciDeviceInfo::empty(); MAX_PCI_DEVICES],
    };

    let start_bus = if use_ecam { acpi.pcie_start_bus } else { 0 };
    let end_bus = if use_ecam {
        acpi.pcie_end_bus.min(start_bus.saturating_add(4))
    } else {
        2
    };

    unsafe {
        for bus in start_bus..=end_bus {
            for dev in 0u8..32 {
                let id0 = if use_ecam {
                    pci_read_u32_ecam(acpi.pcie_ecam_base, bus, dev, 0, 0x00)
                } else {
                    pci_read_u32_legacy(bus, dev, 0, 0x00)
                };
                let vendor0 = (id0 & 0xFFFF) as u16;
                if vendor0 == 0xFFFF || vendor0 == 0x0000 {
                    continue;
                }

                let hdr_reg = if use_ecam {
                    pci_read_u32_ecam(acpi.pcie_ecam_base, bus, dev, 0, 0x0C)
                } else {
                    pci_read_u32_legacy(bus, dev, 0, 0x0C)
                };
                let header_type = ((hdr_reg >> 16) & 0xFF) as u8;
                let max_func = if (header_type & 0x80) != 0 { 8u8 } else { 1u8 };

                for func in 0u8..max_func {
                    if result.device_count >= MAX_PCI_DEVICES {
                        return result;
                    }

                    let id = if func == 0 {
                        id0
                    } else if use_ecam {
                        pci_read_u32_ecam(acpi.pcie_ecam_base, bus, dev, func, 0x00)
                    } else {
                        pci_read_u32_legacy(bus, dev, func, 0x00)
                    };
                    let vendor_id = (id & 0xFFFF) as u16;
                    let device_id = ((id >> 16) & 0xFFFF) as u16;
                    if vendor_id == 0xFFFF || vendor_id == 0x0000 {
                        continue;
                    }

                    let class_reg = if use_ecam {
                        pci_read_u32_ecam(acpi.pcie_ecam_base, bus, dev, func, 0x08)
                    } else {
                        pci_read_u32_legacy(bus, dev, func, 0x08)
                    };
                    let class_code = ((class_reg >> 24) & 0xFF) as u8;
                    let subclass = ((class_reg >> 16) & 0xFF) as u8;
                    let prog_if = ((class_reg >> 8) & 0xFF) as u8;

                    let bar0_low = if use_ecam {
                        pci_read_u32_ecam(acpi.pcie_ecam_base, bus, dev, func, 0x10)
                    } else {
                        pci_read_u32_legacy(bus, dev, func, 0x10)
                    };
                    let is_64bit_bar = (bar0_low & 0x01) == 0 && ((bar0_low >> 1) & 0x03) == 0x02;
                    let bar0: u64 = if is_64bit_bar {
                        let bar0_high = if use_ecam {
                            pci_read_u32_ecam(acpi.pcie_ecam_base, bus, dev, func, 0x14)
                        } else {
                            pci_read_u32_legacy(bus, dev, func, 0x14)
                        };
                        ((bar0_high as u64) << 32) | ((bar0_low & !0xFu32) as u64)
                    } else {
                        (bar0_low & !0xFu32) as u64
                    };

                    let irq_reg = if use_ecam {
                        pci_read_u32_ecam(acpi.pcie_ecam_base, bus, dev, func, 0x3C)
                    } else {
                        pci_read_u32_legacy(bus, dev, func, 0x3C)
                    };
                    let irq_line = (irq_reg & 0xFF) as u8;

                    let storage_kind =
                        classify_storage(vendor_id, device_id, class_code, subclass, prog_if);
                    if storage_kind != StorageKind::None {
                        result.storage_controllers += 1;
                    }

                    result.devices[result.device_count] = PciDeviceInfo {
                        bus,
                        device: dev,
                        function: func,
                        vendor_id,
                        device_id,
                        class_code,
                        subclass,
                        prog_if,
                        bar0,
                        irq_line,
                        storage_kind,
                    };
                    result.device_count += 1;
                }
            }
        }
    }

    result
}
