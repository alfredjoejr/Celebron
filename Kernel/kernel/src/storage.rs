//! Celebron OS Block Storage Abstraction, GPT Partition Parser & CelebronFS Volume Probe.
//! Bridges Hyper-V VMBus `StorVSC` SCSI block devices to Celebron's microkernel,
//! validates GUID Partition Tables (GPT), verifies persistent read/write access on the
//! `CelebronFS CoW Root` partition via an incrementing CRC32-protected mount counter,
//! and enforces strict partition bounds for capability-gated Ring-3 block syscalls.

use crate::hyperv::{self, HyperVStatus};
use core::ptr::addr_of_mut;

const SECTOR_SIZE: usize = 512;

/// EFI System Partition Type GUID (`C12A7328-F81F-11D2-BA4B-00A0C93EC93B` mixed-endian).
const GUID_ESP: [u8; 16] = [
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11,
    0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
];

/// CelebronFS CoW Root Type GUID (`CE1EB201-F500-4C4F-A100-CE1EB2012026` mixed-endian).
const GUID_CELEBRONFS: [u8; 16] = [
    0x01, 0xB2, 0x1E, 0xCE, 0x00, 0xF5, 0x4F, 0x4C,
    0xA1, 0x00, 0xCE, 0x1E, 0xB2, 0x01, 0x20, 0x26,
];

const CELEBRONFS_MAGIC: &[u8; 16] = b"CELEBRONFS_V1\0\0\0";

#[derive(Debug, Clone, Copy)]
pub struct StorageInfo {
    pub disk_online: bool,
    pub total_sectors: u64,
    pub sector_size: u32,
    pub gpt_valid: bool,
    pub partition_count: usize,
    pub esp_start_lba: u64,
    pub esp_sectors: u64,
    pub cfs_found: bool,
    pub cfs_start_lba: u64,
    pub cfs_sectors: u64,
    pub cfs_header_valid: bool,
    pub cfs_mount_count: u64,
    pub cfs_write_verified: bool,
}

impl StorageInfo {
    pub const fn offline() -> Self {
        Self {
            disk_online: false,
            total_sectors: 0,
            sector_size: SECTOR_SIZE as u32,
            gpt_valid: false,
            partition_count: 0,
            esp_start_lba: 0,
            esp_sectors: 0,
            cfs_found: false,
            cfs_start_lba: 0,
            cfs_sectors: 0,
            cfs_header_valid: false,
            cfs_mount_count: 0,
            cfs_write_verified: false,
        }
    }
}

struct StorageState {
    cfs_ready: bool,
    cfs_start_lba: u64,
    cfs_sectors: u64,
}

static mut STORAGE_STATE: StorageState = StorageState {
    cfs_ready: false,
    cfs_start_lba: 0,
    cfs_sectors: 0,
};

/// Standard IEEE 802.3 CRC-32 (`0xEDB88320`, matches `VM/disk-builder`).
pub fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            if (crc & 1) != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// Translate and strictly bounds-check a CelebronFS partition-relative sector request
/// into a physical disk LBA so callers can never escape into GPT or ESP sectors.
fn translate_cfs_lba(rel_lba: u64, count: u16) -> Result<u64, &'static str> {
    let st = unsafe { &*addr_of_mut!(STORAGE_STATE) };
    if !st.cfs_ready {
        return Err("CelebronFS partition not mounted");
    }
    if count == 0 {
        return Err("Sector count must be non-zero");
    }
    let end_rel = rel_lba
        .checked_add(count as u64)
        .ok_or("CelebronFS LBA overflow")?;
    if end_rel > st.cfs_sectors {
        return Err("CelebronFS partition LBA out of bounds");
    }
    Ok(st.cfs_start_lba + rel_lba)
}

/// Read `count` sectors relative to the start of the `CelebronFS CoW Root` partition.
pub fn read_cfs_sectors(rel_lba: u64, count: u16, out_buf: &mut [u8]) -> Result<(), &'static str> {
    let phys_lba = translate_cfs_lba(rel_lba, count)?;
    hyperv::storvsc_read_sectors(phys_lba, count, out_buf)
}

/// Write `count` sectors relative to the start of the `CelebronFS CoW Root` partition.
pub fn write_cfs_sectors(rel_lba: u64, count: u16, in_buf: &[u8]) -> Result<(), &'static str> {
    let phys_lba = translate_cfs_lba(rel_lba, count)?;
    hyperv::storvsc_write_sectors(phys_lba, count, in_buf)
}

fn is_zero_guid(guid: &[u8]) -> bool {
    guid.iter().all(|&b| b == 0)
}

/// Initialize the block layer from discovered disks, parse GPT, verify `CelebronFS` volume
/// header + CRC-32, and run a persistent mount-counter read/write/read-back check.
pub fn init(hv_status: &HyperVStatus) -> StorageInfo {
    if !hv_status.storvsc_disk.online {
        crate::kprintln!("[CELEBRON-STOR] No online StorVSC block disk detected; storage standby.");
        return StorageInfo::offline();
    }

    let mut info = StorageInfo::offline();
    info.disk_online = true;
    info.total_sectors = hv_status.storvsc_disk.sector_count;
    info.sector_size = hv_status.storvsc_disk.sector_size;

    // 1. Read and validate Primary GPT Header at LBA 1
    let mut gpt_hdr = [0u8; SECTOR_SIZE];
    if let Err(err) = hyperv::storvsc_read_sectors(1, 1, &mut gpt_hdr) {
        crate::kprintln!("[CELEBRON-STOR] Failed to read GPT header (LBA 1): {}", err);
        return info;
    }

    if &gpt_hdr[0..8] != b"EFI PART" {
        crate::kprintln!("[CELEBRON-STOR] LBA 1 missing 'EFI PART' signature");
        return info;
    }

    let hdr_size = u32::from_le_bytes([gpt_hdr[12], gpt_hdr[13], gpt_hdr[14], gpt_hdr[15]]) as usize;
    if hdr_size < 92 || hdr_size > SECTOR_SIZE {
        crate::kprintln!("[CELEBRON-STOR] Invalid GPT header size: {}", hdr_size);
        return info;
    }

    let stored_crc = u32::from_le_bytes([gpt_hdr[16], gpt_hdr[17], gpt_hdr[18], gpt_hdr[19]]);
    let mut crc_buf = gpt_hdr;
    crc_buf[16..20].fill(0);
    if crc32_ieee(&crc_buf[..hdr_size]) != stored_crc {
        crate::kprintln!("[CELEBRON-STOR] GPT header CRC-32 mismatch");
        return info;
    }

    let part_entries_lba = u64::from_le_bytes([
        gpt_hdr[72], gpt_hdr[73], gpt_hdr[74], gpt_hdr[75],
        gpt_hdr[76], gpt_hdr[77], gpt_hdr[78], gpt_hdr[79],
    ]);
    let num_entries =
        u32::from_le_bytes([gpt_hdr[80], gpt_hdr[81], gpt_hdr[82], gpt_hdr[83]]) as usize;
    let entry_size =
        u32::from_le_bytes([gpt_hdr[84], gpt_hdr[85], gpt_hdr[86], gpt_hdr[87]]) as usize;
    if entry_size < 128 || entry_size > 512 {
        crate::kprintln!("[CELEBRON-STOR] Unsupported GPT entry size: {}", entry_size);
        return info;
    }

    info.gpt_valid = true;

    // 2. Read first 4 sectors (2048 B = 16 standard GPT entries) from `part_entries_lba`
    let mut entries_buf = [0u8; 4 * SECTOR_SIZE];
    if let Err(err) = hyperv::storvsc_read_sectors(part_entries_lba, 4, &mut entries_buf) {
        crate::kprintln!("[CELEBRON-STOR] Failed to read GPT partition entries: {}", err);
        return info;
    }

    let scan_limit = num_entries.min(entries_buf.len() / entry_size);
    for idx in 0..scan_limit {
        let off = idx * entry_size;
        let entry = &entries_buf[off..off + entry_size];
        let type_guid = &entry[0..16];
        if is_zero_guid(type_guid) {
            continue;
        }

        let start_lba = u64::from_le_bytes([
            entry[32], entry[33], entry[34], entry[35],
            entry[36], entry[37], entry[38], entry[39],
        ]);
        let end_lba = u64::from_le_bytes([
            entry[40], entry[41], entry[42], entry[43],
            entry[44], entry[45], entry[46], entry[47],
        ]);
        if end_lba < start_lba || end_lba >= info.total_sectors {
            continue;
        }
        let sectors = end_lba - start_lba + 1;
        info.partition_count += 1;

        if type_guid == GUID_ESP {
            info.esp_start_lba = start_lba;
            info.esp_sectors = sectors;
        } else if type_guid == GUID_CELEBRONFS {
            info.cfs_found = true;
            info.cfs_start_lba = start_lba;
            info.cfs_sectors = sectors;
        }
    }

    crate::kprintln!(
        "[CELEBRON-STOR] GPT valid: {} partition(s) | ESP LBA={} ({} sec) | CelebronFS found={} (LBA={}, {} sec)",
        info.partition_count,
        info.esp_start_lba,
        info.esp_sectors,
        info.cfs_found,
        info.cfs_start_lba,
        info.cfs_sectors
    );

    if !info.cfs_found {
        return info;
    }

    unsafe {
        let st = &mut *addr_of_mut!(STORAGE_STATE);
        st.cfs_ready = true;
        st.cfs_start_lba = info.cfs_start_lba;
        st.cfs_sectors = info.cfs_sectors;
    }

    // 3. Verify CelebronFS Volume Header & run persistent mount counter Write + Read-Back
    let mut cfs_sec = [0u8; SECTOR_SIZE];
    if let Err(err) = read_cfs_sectors(0, 1, &mut cfs_sec) {
        crate::kprintln!("[CELEBRON-STOR] Failed to read CelebronFS volume header: {}", err);
        return info;
    }

    if &cfs_sec[0..16] != CELEBRONFS_MAGIC {
        crate::kprintln!("[CELEBRON-STOR] Invalid CelebronFS volume magic");
        return info;
    }

    let hdr_crc = u32::from_le_bytes([cfs_sec[64], cfs_sec[65], cfs_sec[66], cfs_sec[67]]);
    if crc32_ieee(&cfs_sec[0..64]) != hdr_crc {
        crate::kprintln!("[CELEBRON-STOR] CelebronFS volume header CRC-32 mismatch");
        return info;
    }

    info.cfs_header_valid = true;
    let prev_mounts = u64::from_le_bytes([
        cfs_sec[48], cfs_sec[49], cfs_sec[50], cfs_sec[51],
        cfs_sec[52], cfs_sec[53], cfs_sec[54], cfs_sec[55],
    ]);
    let next_mounts = prev_mounts.saturating_add(1);
    cfs_sec[48..56].copy_from_slice(&next_mounts.to_le_bytes());
    let new_crc = crc32_ieee(&cfs_sec[0..64]);
    cfs_sec[64..68].copy_from_slice(&new_crc.to_le_bytes());

    if let Err(err) = write_cfs_sectors(0, 1, &cfs_sec) {
        crate::kprintln!("[CELEBRON-STOR] Failed to persist CelebronFS mount_count: {}", err);
        info.cfs_mount_count = prev_mounts;
        return info;
    }

    // Read-back verification from physical storage
    let mut verify_sec = [0u8; SECTOR_SIZE];
    if let Err(err) = read_cfs_sectors(0, 1, &mut verify_sec) {
        crate::kprintln!("[CELEBRON-STOR] Failed to read-back CelebronFS header: {}", err);
        info.cfs_mount_count = prev_mounts;
        return info;
    }

    let verify_mounts = u64::from_le_bytes([
        verify_sec[48], verify_sec[49], verify_sec[50], verify_sec[51],
        verify_sec[52], verify_sec[53], verify_sec[54], verify_sec[55],
    ]);
    let verify_crc =
        u32::from_le_bytes([verify_sec[64], verify_sec[65], verify_sec[66], verify_sec[67]]);

    if verify_mounts == next_mounts && verify_crc == new_crc && crc32_ieee(&verify_sec[0..64]) == new_crc {
        info.cfs_mount_count = verify_mounts;
        info.cfs_write_verified = true;
        crate::kprintln!(
            "[CELEBRON-STOR] CelebronFS persistence verified! mount_count={} (sector_size={} B, RW=OK)",
            info.cfs_mount_count,
            info.sector_size
        );
    } else {
        info.cfs_mount_count = prev_mounts;
        crate::kprintln!("[CELEBRON-STOR] CelebronFS read-back verification mismatch");
    }

    info
}
