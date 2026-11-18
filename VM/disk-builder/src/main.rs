//! Celebron OS — Self-Contained UEFI GPT + FAT32 + VHD + VHDX Disk Image Builder.
//! Packages `BOOTX64.EFI` and `KERNEL.ELF` into bootable virtual hard disks for
//! Windows Hyper-V (Generation 2 `.vhdx`), VirtualBox/Hyper-V (`.vhd`), and QEMU (`.img`).

use std::env;
use std::fs;
use std::path::PathBuf;

const SECTOR_SIZE: usize = 512;
const DISK_SIZE_BYTES: usize = 64 * 1024 * 1024; // 64 MiB
const TOTAL_SECTORS: u64 = (DISK_SIZE_BYTES / SECTOR_SIZE) as u64; // 131,072 sectors

const ESP_START_LBA: u64 = 2048;
const ESP_END_LBA: u64 = 130_047;
const ESP_SECTORS: u32 = (ESP_END_LBA - ESP_START_LBA + 1) as u32; // 128,000 sectors

const FAT_RESERVED_SECTORS: u32 = 32;
const FAT_COUNT: u32 = 2;
const SECTORS_PER_FAT: u32 = 1000;

/// Standard IEEE 802.3 CRC-32 (used by GPT headers and partition tables).
fn crc32_ieee(data: &[u8]) -> u32 {
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

/// Castagnoli CRC-32C (polynomial `0x82F63B78`, used by Microsoft VHDX 1.0 headers & region tables).
fn crc32c(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            if (crc & 1) != 0 {
                crc = (crc >> 1) ^ 0x82F6_3B78;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// Write a Protective MBR at LBA 0 covering the entire 64 MiB GPT disk.
fn write_protective_mbr(disk: &mut [u8]) {
    let mbr = &mut disk[0..SECTOR_SIZE];
    // Partition Entry 1 at offset 446
    let entry = &mut mbr[446..462];
    entry[0] = 0x00; // Not legacy-bootable (UEFI uses GPT)
    entry[1] = 0x00;
    entry[2] = 0x02;
    entry[3] = 0x00;
    entry[4] = 0xEE; // GPT Protective MBR type
    entry[5] = 0xFF;
    entry[6] = 0xFF;
    entry[7] = 0xFF;
    entry[8..12].copy_from_slice(&1u32.to_le_bytes()); // LBA start = 1
    let size_lba = (TOTAL_SECTORS - 1) as u32;
    entry[12..16].copy_from_slice(&size_lba.to_le_bytes());

    mbr[510] = 0x55;
    mbr[511] = 0xAA;
}

/// Write Primary and Backup GPT headers and the EFI System Partition entry.
fn write_gpt(disk: &mut [u8]) {
    // Build the 128-entry (16 KiB = 32 sectors) partition entry array
    let mut entries = vec![0u8; 128 * 128];

    // Entry 0: EFI System Partition (C12A7328-F81F-11D2-BA4B-00A0C93EC93B in mixed-endian)
    let esp_type_guid: [u8; 16] = [
        0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11,
        0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
    ];
    let esp_unique_guid: [u8; 16] = [
        0xCE, 0x1E, 0xB2, 0x01, 0x20, 0x26, 0x4F, 0x53,
        0x9A, 0xBC, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06,
    ];

    entries[0..16].copy_from_slice(&esp_type_guid);
    entries[16..32].copy_from_slice(&esp_unique_guid);
    entries[32..40].copy_from_slice(&ESP_START_LBA.to_le_bytes());
    entries[40..48].copy_from_slice(&ESP_END_LBA.to_le_bytes());
    entries[48..56].copy_from_slice(&0u64.to_le_bytes()); // Attributes

    let name = "Celebron EFI System";
    for (i, ch) in name.encode_utf16().enumerate() {
        let off = 56 + i * 2;
        entries[off..off + 2].copy_from_slice(&ch.to_le_bytes());
    }

    let entries_crc = crc32_ieee(&entries);

    // Copy Primary Partition Entries to LBA 2..34
    let primary_entries_offset = 2 * SECTOR_SIZE;
    disk[primary_entries_offset..primary_entries_offset + entries.len()].copy_from_slice(&entries);

    // Copy Backup Partition Entries to LBA (TOTAL_SECTORS - 33)..(TOTAL_SECTORS - 1)
    let backup_entries_lba = TOTAL_SECTORS - 33;
    let backup_entries_offset = (backup_entries_lba as usize) * SECTOR_SIZE;
    disk[backup_entries_offset..backup_entries_offset + entries.len()].copy_from_slice(&entries);

    let disk_guid: [u8; 16] = [
        0x43, 0x45, 0x4C, 0x45, 0x42, 0x52, 0x4F, 0x4E,
        0xAA, 0x55, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05,
    ];

    // Helper closure to build a 512-byte GPT header sector
    let build_gpt_header = |my_lba: u64, alt_lba: u64, part_entries_lba: u64| -> [u8; SECTOR_SIZE] {
        let mut hdr = [0u8; SECTOR_SIZE];
        hdr[0..8].copy_from_slice(b"EFI PART");
        hdr[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes()); // Revision 1.0
        hdr[12..16].copy_from_slice(&92u32.to_le_bytes()); // Header size = 92
        hdr[16..20].copy_from_slice(&0u32.to_le_bytes()); // CRC32 placeholder
        hdr[20..24].copy_from_slice(&0u32.to_le_bytes()); // Reserved
        hdr[24..32].copy_from_slice(&my_lba.to_le_bytes());
        hdr[32..40].copy_from_slice(&alt_lba.to_le_bytes());
        hdr[40..48].copy_from_slice(&34u64.to_le_bytes()); // First usable LBA
        hdr[48..56].copy_from_slice(&(TOTAL_SECTORS - 34).to_le_bytes()); // Last usable LBA
        hdr[56..72].copy_from_slice(&disk_guid);
        hdr[72..80].copy_from_slice(&part_entries_lba.to_le_bytes());
        hdr[80..84].copy_from_slice(&128u32.to_le_bytes()); // Number of partition entries
        hdr[84..88].copy_from_slice(&128u32.to_le_bytes()); // Size of each entry
        hdr[88..92].copy_from_slice(&entries_crc.to_le_bytes());

        let hdr_crc = crc32_ieee(&hdr[0..92]);
        hdr[16..20].copy_from_slice(&hdr_crc.to_le_bytes());
        hdr
    };

    // Primary GPT header at LBA 1
    let primary_hdr = build_gpt_header(1, TOTAL_SECTORS - 1, 2);
    disk[SECTOR_SIZE..2 * SECTOR_SIZE].copy_from_slice(&primary_hdr);

    // Backup GPT header at LBA (TOTAL_SECTORS - 1)
    let backup_hdr = build_gpt_header(TOTAL_SECTORS - 1, 1, backup_entries_lba);
    let last_lba_offset = ((TOTAL_SECTORS - 1) as usize) * SECTOR_SIZE;
    disk[last_lba_offset..last_lba_offset + SECTOR_SIZE].copy_from_slice(&backup_hdr);
}

/// Write a 32-byte FAT32 8.3 directory entry into `dir_buf`.
fn write_fat_dir_entry(
    dir_buf: &mut [u8],
    name_8_3: &[u8; 11],
    attr: u8,
    first_cluster: u32,
    file_size: u32,
) {
    dir_buf[0..11].copy_from_slice(name_8_3);
    dir_buf[11] = attr;
    let hi = ((first_cluster >> 16) & 0xFFFF) as u16;
    let lo = (first_cluster & 0xFFFF) as u16;
    dir_buf[20..22].copy_from_slice(&hi.to_le_bytes());
    dir_buf[26..28].copy_from_slice(&lo.to_le_bytes());
    dir_buf[28..32].copy_from_slice(&file_size.to_le_bytes());
}

/// Format the EFI System Partition as FAT32 and populate `\EFI\BOOT\BOOTX64.EFI` and `\EFI\CELEBRON\KERNEL.ELF`.
fn write_fat32_esp(disk: &mut [u8], bootloader_efi: &[u8], kernel_elf: &[u8]) {
    let esp_offset = (ESP_START_LBA as usize) * SECTOR_SIZE;
    let esp = &mut disk[esp_offset..esp_offset + (ESP_SECTORS as usize) * SECTOR_SIZE];

    // 1. Build FAT32 Boot Sector (BPB)
    let mut bpb = [0u8; SECTOR_SIZE];
    bpb[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
    bpb[3..11].copy_from_slice(b"CELEBRON");
    bpb[11..13].copy_from_slice(&(SECTOR_SIZE as u16).to_le_bytes()); // 512 bytes/sector
    bpb[13] = 1; // 1 sector per cluster => 125,968 data clusters (> 65,525 strict FAT32 threshold)
    bpb[14..16].copy_from_slice(&(FAT_RESERVED_SECTORS as u16).to_le_bytes());
    bpb[16] = FAT_COUNT as u8;
    bpb[17..19].copy_from_slice(&0u16.to_le_bytes()); // 0 for FAT32
    bpb[19..21].copy_from_slice(&0u16.to_le_bytes()); // 0 for FAT32
    bpb[21] = 0xF8; // Fixed disk media descriptor
    bpb[22..24].copy_from_slice(&0u16.to_le_bytes()); // 0 for FAT32
    bpb[24..26].copy_from_slice(&63u16.to_le_bytes()); // Sectors per track
    bpb[26..28].copy_from_slice(&255u16.to_le_bytes()); // Heads
    bpb[28..32].copy_from_slice(&(ESP_START_LBA as u32).to_le_bytes()); // Hidden sectors
    bpb[32..36].copy_from_slice(&ESP_SECTORS.to_le_bytes()); // Total sectors (128,000)

    // FAT32 Extended BPB
    bpb[36..40].copy_from_slice(&SECTORS_PER_FAT.to_le_bytes()); // Sectors per FAT (1000)
    bpb[40..42].copy_from_slice(&0u16.to_le_bytes()); // Ext flags
    bpb[42..44].copy_from_slice(&0u16.to_le_bytes()); // FS version 0.0
    bpb[44..48].copy_from_slice(&2u32.to_le_bytes()); // Root directory cluster = 2
    bpb[48..50].copy_from_slice(&1u16.to_le_bytes()); // FSInfo sector = 1
    bpb[50..52].copy_from_slice(&6u16.to_le_bytes()); // Backup boot sector = 6
    bpb[64] = 0x80; // Drive number
    bpb[66] = 0x29; // Extended boot signature
    bpb[67..71].copy_from_slice(&0xCE1E_B201u32.to_le_bytes()); // Volume serial
    bpb[71..82].copy_from_slice(b"CELEBRON_OS");
    bpb[82..90].copy_from_slice(b"FAT32   ");
    bpb[510] = 0x55;
    bpb[511] = 0xAA;

    // 2. Build FSInfo Sector (Sector 1)
    let mut fsinfo = [0u8; SECTOR_SIZE];
    fsinfo[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
    fsinfo[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
    fsinfo[488..492].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // Free cluster count unknown
    fsinfo[492..496].copy_from_slice(&2u32.to_le_bytes()); // Next free cluster hint
    fsinfo[510] = 0x55;
    fsinfo[511] = 0xAA;

    // Write Primary & Backup Boot + FSInfo sectors
    esp[0..SECTOR_SIZE].copy_from_slice(&bpb);
    esp[SECTOR_SIZE..2 * SECTOR_SIZE].copy_from_slice(&fsinfo);
    esp[6 * SECTOR_SIZE..7 * SECTOR_SIZE].copy_from_slice(&bpb);
    esp[7 * SECTOR_SIZE..8 * SECTOR_SIZE].copy_from_slice(&fsinfo);

    // 3. Allocate clusters for directories & files:
    // Cluster 2: Root directory `\`
    // Cluster 3: `\EFI`
    // Cluster 4: `\EFI\BOOT`
    // Cluster 5: `\EFI\CELEBRON`
    // Cluster 6..(6 + boot_clusters - 1): `BOOTX64.EFI`
    // Cluster (6 + boot_clusters)..: `KERNEL.ELF`
    let boot_clusters = ((bootloader_efi.len() + SECTOR_SIZE - 1) / SECTOR_SIZE).max(1) as u32;
    let kernel_clusters = ((kernel_elf.len() + SECTOR_SIZE - 1) / SECTOR_SIZE).max(1) as u32;
    let boot_first_cluster: u32 = 6;
    let kernel_first_cluster: u32 = boot_first_cluster + boot_clusters;

    // Build FAT table in memory
    let mut fat = vec![0u8; (SECTORS_PER_FAT as usize) * SECTOR_SIZE];
    let set_fat_entry = |fat_buf: &mut [u8], cluster: u32, value: u32| {
        let off = (cluster as usize) * 4;
        fat_buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
    };

    // Reserved FAT entries 0 & 1, plus directory single-cluster EOF markers (clusters 2..=5)
    set_fat_entry(&mut fat, 0, 0x0FFF_FFF8);
    set_fat_entry(&mut fat, 1, 0x0FFF_FFFF);
    for dir_cluster in 2..=5 {
        set_fat_entry(&mut fat, dir_cluster, 0x0FFF_FFFF);
    }

    // Chain clusters for BOOTX64.EFI
    for i in 0..boot_clusters {
        let c = boot_first_cluster + i;
        let next = if i + 1 == boot_clusters {
            0x0FFF_FFFF
        } else {
            c + 1
        };
        set_fat_entry(&mut fat, c, next);
    }

    // Chain clusters for KERNEL.ELF
    for i in 0..kernel_clusters {
        let c = kernel_first_cluster + i;
        let next = if i + 1 == kernel_clusters {
            0x0FFF_FFFF
        } else {
            c + 1
        };
        set_fat_entry(&mut fat, c, next);
    }

    // Copy FAT to FAT #1 and FAT #2
    let fat1_offset = (FAT_RESERVED_SECTORS as usize) * SECTOR_SIZE;
    let fat2_offset = fat1_offset + (SECTORS_PER_FAT as usize) * SECTOR_SIZE;
    esp[fat1_offset..fat1_offset + fat.len()].copy_from_slice(&fat);
    esp[fat2_offset..fat2_offset + fat.len()].copy_from_slice(&fat);

    // Helper to get byte offset of a data cluster inside `esp`
    let data_start_sector = (FAT_RESERVED_SECTORS + FAT_COUNT * SECTORS_PER_FAT) as usize;
    let cluster_offset = |cluster: u32| -> usize {
        (data_start_sector + ((cluster - 2) as usize)) * SECTOR_SIZE
    };

    // Populate Cluster 2 (Root Directory `\`)
    {
        let off = cluster_offset(2);
        let root_dir = &mut esp[off..off + SECTOR_SIZE];
        write_fat_dir_entry(&mut root_dir[0..32], b"CELEBRON_OS", 0x08, 0, 0); // Volume label
        write_fat_dir_entry(&mut root_dir[32..64], b"EFI        ", 0x10, 3, 0);
    }

    // Populate Cluster 3 (`\EFI`)
    {
        let off = cluster_offset(3);
        let efi_dir = &mut esp[off..off + SECTOR_SIZE];
        write_fat_dir_entry(&mut efi_dir[0..32], b".          ", 0x10, 3, 0);
        write_fat_dir_entry(&mut efi_dir[32..64], b"..         ", 0x10, 0, 0);
        write_fat_dir_entry(&mut efi_dir[64..96], b"BOOT       ", 0x10, 4, 0);
        write_fat_dir_entry(&mut efi_dir[96..128], b"CELEBRON   ", 0x10, 5, 0);
    }

    // Populate Cluster 4 (`\EFI\BOOT`)
    {
        let off = cluster_offset(4);
        let boot_dir = &mut esp[off..off + SECTOR_SIZE];
        write_fat_dir_entry(&mut boot_dir[0..32], b".          ", 0x10, 4, 0);
        write_fat_dir_entry(&mut boot_dir[32..64], b"..         ", 0x10, 3, 0);
        write_fat_dir_entry(
            &mut boot_dir[64..96],
            b"BOOTX64 EFI",
            0x20,
            boot_first_cluster,
            bootloader_efi.len() as u32,
        );
    }

    // Populate Cluster 5 (`\EFI\CELEBRON`)
    {
        let off = cluster_offset(5);
        let cel_dir = &mut esp[off..off + SECTOR_SIZE];
        write_fat_dir_entry(&mut cel_dir[0..32], b".          ", 0x10, 5, 0);
        write_fat_dir_entry(&mut cel_dir[32..64], b"..         ", 0x10, 3, 0);
        write_fat_dir_entry(
            &mut cel_dir[64..96],
            b"KERNEL  ELF",
            0x20,
            kernel_first_cluster,
            kernel_elf.len() as u32,
        );
    }

    // Write `BOOTX64.EFI` data into clusters starting at `boot_first_cluster`
    let boot_data_off = cluster_offset(boot_first_cluster);
    esp[boot_data_off..boot_data_off + bootloader_efi.len()].copy_from_slice(bootloader_efi);

    // Write `KERNEL.ELF` data into clusters starting at `kernel_first_cluster`
    let kernel_data_off = cluster_offset(kernel_first_cluster);
    esp[kernel_data_off..kernel_data_off + kernel_elf.len()].copy_from_slice(kernel_elf);
}

/// Build the 512-byte Microsoft Fixed VHD footer (`conectix`) so Hyper-V / VirtualBox recognize `.vhd`.
fn build_vhd_footer(disk_size: u64) -> [u8; SECTOR_SIZE] {
    let mut footer = [0u8; SECTOR_SIZE];
    footer[0..8].copy_from_slice(b"conectix");
    footer[8..12].copy_from_slice(&2u32.to_be_bytes()); // Features = Reserved (2)
    footer[12..16].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // Format version 1.0
    footer[16..24].copy_from_slice(&u64::MAX.to_be_bytes()); // Fixed disk data offset = 0xFFFFFFFFFFFFFFFF
    footer[24..28].copy_from_slice(&0u32.to_be_bytes()); // Timestamp
    footer[28..32].copy_from_slice(b"celb"); // Creator application
    footer[32..36].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // Creator version
    footer[36..40].copy_from_slice(b"Wi2k"); // Creator host OS = Windows
    footer[40..48].copy_from_slice(&disk_size.to_be_bytes()); // Original size
    footer[48..56].copy_from_slice(&disk_size.to_be_bytes()); // Current size

    // Disk Geometry (CHS for 64 MiB: 130 cylinders, 16 heads, 63 sectors/track)
    let cylinders: u16 = 130;
    footer[56..58].copy_from_slice(&cylinders.to_be_bytes());
    footer[58] = 16;
    footer[59] = 63;
    footer[60..64].copy_from_slice(&2u32.to_be_bytes()); // Disk type = Fixed hard disk (2)

    // Unique ID
    footer[68..84].copy_from_slice(&[
        0x43, 0x45, 0x4C, 0x45, 0x42, 0x52, 0x4F, 0x4E,
        0x56, 0x48, 0x44, 0x00, 0x00, 0x00, 0x00, 0x01,
    ]);

    // Compute ones'-complement checksum over all 512 bytes (with checksum field 64..68 zeroed)
    let sum: u32 = footer.iter().map(|&b| b as u32).sum();
    let checksum = !sum;
    footer[64..68].copy_from_slice(&checksum.to_be_bytes());
    footer
}

/// Build a native Microsoft VHDX 1.0 Fixed Virtual Hard Disk image (`.vhdx`) directly in Rust
/// for Windows Hyper-V Generation 2 (UEFI) Virtual Machines.
fn build_vhdx_image(raw_disk: &[u8]) -> Vec<u8> {
    const MB: usize = 1024 * 1024;
    const PAYLOAD_OFFSET: usize = 4 * MB; // 1MB Header + 1MB Log + 1MB BAT + 1MB Metadata
    const BLOCK_SIZE: u32 = 32 * (MB as u32); // 32 MiB payload blocks

    let mut vhdx = vec![0u8; PAYLOAD_OFFSET + raw_disk.len()];

    // 1. File Type Identifier at offset 0 (`"vhdxfile"`)
    vhdx[0..8].copy_from_slice(b"vhdxfile");
    for (i, ch) in "CelebronDiskBuilder".encode_utf16().enumerate() {
        let off = 8 + i * 2;
        vhdx[off..off + 2].copy_from_slice(&ch.to_le_bytes());
    }

    // Helper to build Header 1 (at 64 KiB) and Header 2 (at 128 KiB)
    let build_vhdx_header = |seq_num: u64| -> [u8; 4096] {
        let mut h = [0u8; 4096];
        h[0..4].copy_from_slice(b"head");
        h[4..8].copy_from_slice(&0u32.to_le_bytes()); // Checksum placeholder
        h[8..16].copy_from_slice(&seq_num.to_le_bytes());
        // FileWriteGuid
        h[16..32].copy_from_slice(&[
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88,
            0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x01,
        ]);
        // DataWriteGuid
        h[32..48].copy_from_slice(&[
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88,
            0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x02,
        ]);
        // LogGuid = 0 (clean unmounted VHDX state, no log replay required)
        h[48..64].copy_from_slice(&[0u8; 16]);
        h[64..66].copy_from_slice(&0u16.to_le_bytes()); // LogVersion = 0
        h[66..68].copy_from_slice(&1u16.to_le_bytes()); // Version = 1
        h[68..72].copy_from_slice(&(MB as u32).to_le_bytes()); // LogLength = 1 MiB
        h[72..80].copy_from_slice(&(MB as u64).to_le_bytes()); // LogOffset = 1 MiB

        let crc = crc32c(&h);
        h[4..8].copy_from_slice(&crc.to_le_bytes());
        h
    };

    let h1 = build_vhdx_header(1);
    let h2 = build_vhdx_header(2);
    vhdx[0x10000..0x11000].copy_from_slice(&h1);
    vhdx[0x20000..0x21000].copy_from_slice(&h2);

    // 2. Region Tables at 192 KiB (`0x30000`) and 256 KiB (`0x40000`)
    let mut reg = vec![0u8; 65536];
    reg[0..4].copy_from_slice(b"regi");
    reg[4..8].copy_from_slice(&0u32.to_le_bytes()); // Checksum placeholder
    reg[8..12].copy_from_slice(&2u32.to_le_bytes()); // EntryCount = 2 (BAT + Metadata)
    reg[12..16].copy_from_slice(&0u32.to_le_bytes()); // Reserved

    // Region Entry 0: BAT (GUID `2DC27766-F623-4200-9D64-115E9BFD4A08`)
    let bat_guid: [u8; 16] = [
        0x66, 0x77, 0xC2, 0x2D, 0x23, 0xF6, 0x00, 0x42,
        0x9D, 0x64, 0x11, 0x5E, 0x9B, 0xFD, 0x4A, 0x08,
    ];
    reg[16..32].copy_from_slice(&bat_guid);
    reg[32..40].copy_from_slice(&((2 * MB) as u64).to_le_bytes()); // Offset = 2 MiB
    reg[40..44].copy_from_slice(&(MB as u32).to_le_bytes()); // Length = 1 MiB
    reg[44..48].copy_from_slice(&1u32.to_le_bytes()); // Required = 1

    // Region Entry 1: Metadata (GUID `8B7CA206-4790-4B9A-B8FE-575F050F886E`)
    let meta_guid: [u8; 16] = [
        0x06, 0xA2, 0x7C, 0x8B, 0x90, 0x47, 0x9A, 0x4B,
        0xB8, 0xFE, 0x57, 0x5F, 0x05, 0x0F, 0x88, 0x6E,
    ];
    reg[48..64].copy_from_slice(&meta_guid);
    reg[64..72].copy_from_slice(&((3 * MB) as u64).to_le_bytes()); // Offset = 3 MiB
    reg[72..76].copy_from_slice(&(MB as u32).to_le_bytes()); // Length = 1 MiB
    reg[76..80].copy_from_slice(&1u32.to_le_bytes()); // Required = 1

    let reg_crc = crc32c(&reg);
    reg[4..8].copy_from_slice(&reg_crc.to_le_bytes());
    vhdx[0x30000..0x40000].copy_from_slice(&reg);
    vhdx[0x40000..0x50000].copy_from_slice(&reg);

    // 3. Block Allocation Table (BAT) at 2 MiB (`0x200000`)
    // Two 32 MiB payload blocks (64 MiB total): state = 6 (PAYLOAD_BLOCK_FULLY_PRESENT)
    let bat_offset = 2 * MB;
    let bat_entry_0: u64 = ((4u64) << 20) | 6; // Payload block 0 at 4 MiB
    let bat_entry_1: u64 = ((36u64) << 20) | 6; // Payload block 1 at 36 MiB
    vhdx[bat_offset..bat_offset + 8].copy_from_slice(&bat_entry_0.to_le_bytes());
    vhdx[bat_offset + 8..bat_offset + 16].copy_from_slice(&bat_entry_1.to_le_bytes());

    // 4. Metadata Region at 3 MiB (`0x300000`)
    let meta_base = 3 * MB;
    vhdx[meta_base..meta_base + 8].copy_from_slice(b"metadata");
    vhdx[meta_base + 8..meta_base + 10].copy_from_slice(&0u16.to_le_bytes()); // Reserved
    vhdx[meta_base + 10..meta_base + 12].copy_from_slice(&5u16.to_le_bytes()); // EntryCount = 5

    let write_meta_entry = |buf: &mut [u8], idx: usize, guid: [u8; 16], item_off: u32, item_len: u32, flags: u32| {
        let off = 32 + idx * 32;
        buf[off..off + 16].copy_from_slice(&guid);
        buf[off + 16..off + 20].copy_from_slice(&item_off.to_le_bytes());
        buf[off + 20..off + 24].copy_from_slice(&item_len.to_le_bytes());
        buf[off + 24..off + 28].copy_from_slice(&flags.to_le_bytes());
    };

    // Metadata GUIDs (mixed-endian per Microsoft VHDX specification):
    // 1. File Parameters: CAA16737-FA36-4D43-B3B6-33F0AA44E76B (flags = IsRequired [4])
    write_meta_entry(
        &mut vhdx[meta_base..meta_base + MB],
        0,
        [0x37, 0x67, 0xA1, 0xCA, 0x36, 0xFA, 0x43, 0x4D, 0xB3, 0xB6, 0x33, 0xF0, 0xAA, 0x44, 0xE7, 0x6B],
        0x10000,
        8,
        4,
    );
    // 2. Virtual Disk Size: 2FA54224-CD1B-4876-B211-5DBED83BF4B8 (flags = IsVirtualDisk | IsRequired [6])
    write_meta_entry(
        &mut vhdx[meta_base..meta_base + MB],
        1,
        [0x24, 0x42, 0xA5, 0x2F, 0x1B, 0xCD, 0x76, 0x48, 0xB2, 0x11, 0x5D, 0xBE, 0xD8, 0x3B, 0xF4, 0xB8],
        0x10008,
        8,
        6,
    );
    // 3. Virtual Disk ID (Page 83 Data): BECA12AB-B2E6-4523-93EF-C309E000C746 (flags = IsVirtualDisk | IsRequired [6])
    write_meta_entry(
        &mut vhdx[meta_base..meta_base + MB],
        2,
        [0xAB, 0x12, 0xCA, 0xBE, 0xE6, 0xB2, 0x23, 0x45, 0x93, 0xEF, 0xC3, 0x09, 0xE0, 0x00, 0xC7, 0x46],
        0x10010,
        16,
        6,
    );
    // 4. Logical Sector Size: 8141BF1D-A96F-4709-BA47-F233A8FAAB5F (flags = IsVirtualDisk | IsRequired [6])
    write_meta_entry(
        &mut vhdx[meta_base..meta_base + MB],
        3,
        [0x1D, 0xBF, 0x41, 0x81, 0x6F, 0xA9, 0x09, 0x47, 0xBA, 0x47, 0xF2, 0x33, 0xA8, 0xFA, 0xAB, 0x5F],
        0x10020,
        4,
        6,
    );
    // 5. Physical Sector Size: CDA348C7-445D-4471-9CC9-E9885251C556 (flags = IsVirtualDisk | IsRequired [6])
    write_meta_entry(
        &mut vhdx[meta_base..meta_base + MB],
        4,
        [0xC7, 0x48, 0xA3, 0xCD, 0x5D, 0x44, 0x71, 0x44, 0x9C, 0xC9, 0xE9, 0x88, 0x52, 0x51, 0xC5, 0x56],
        0x10024,
        4,
        6,
    );

    // Metadata values starting at `meta_base + 0x10000`
    let val_base = meta_base + 0x10000;
    // 1. File Parameters (BlockSize = 32 MiB, LeaveBlocksAllocated = 1)
    vhdx[val_base..val_base + 4].copy_from_slice(&BLOCK_SIZE.to_le_bytes());
    vhdx[val_base + 4..val_base + 8].copy_from_slice(&1u32.to_le_bytes());
    // 2. Virtual Disk Size (64 MiB)
    vhdx[val_base + 8..val_base + 16].copy_from_slice(&(DISK_SIZE_BYTES as u64).to_le_bytes());
    // 3. Virtual Disk ID
    vhdx[val_base + 16..val_base + 32].copy_from_slice(&[
        0x43, 0x45, 0x4C, 0x45, 0x42, 0x52, 0x4F, 0x4E,
        0x56, 0x48, 0x44, 0x58, 0x00, 0x00, 0x00, 0x01,
    ]);
    // 4. Logical Sector Size = 512
    vhdx[val_base + 32..val_base + 36].copy_from_slice(&512u32.to_le_bytes());
    // 5. Physical Sector Size = 4096
    vhdx[val_base + 36..val_base + 40].copy_from_slice(&4096u32.to_le_bytes());

    // 5. Payload Data at 4 MiB (`0x400000`)
    vhdx[PAYLOAD_OFFSET..PAYLOAD_OFFSET + raw_disk.len()].copy_from_slice(raw_disk);

    vhdx
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 4 {
        eprintln!(
            "Usage: {} <path-to-BOOTX64.EFI> <path-to-KERNEL.ELF> <output-dir>",
            args[0]
        );
        std::process::exit(1);
    }

    let bootloader_path = PathBuf::from(&args[1]);
    let kernel_path = PathBuf::from(&args[2]);
    let out_dir = PathBuf::from(&args[3]);

    let bootloader_bytes = fs::read(&bootloader_path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {}", bootloader_path.display(), e));
    let kernel_bytes = fs::read(&kernel_path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {}", kernel_path.display(), e));

    println!(
        "[Celebron Disk Builder] Packaging BOOTX64.EFI ({} bytes) + KERNEL.ELF ({} bytes)...",
        bootloader_bytes.len(),
        kernel_bytes.len()
    );

    let mut raw_disk = vec![0u8; DISK_SIZE_BYTES];
    write_protective_mbr(&mut raw_disk);
    write_gpt(&mut raw_disk);
    write_fat32_esp(&mut raw_disk, &bootloader_bytes, &kernel_bytes);

    fs::create_dir_all(&out_dir).expect("Failed to create VM output directory");

    // 1. Write Raw GPT+FAT32 UEFI image (`CelebronOS.img`)
    let img_path = out_dir.join("CelebronOS.img");
    fs::write(&img_path, &raw_disk).expect("Failed to write CelebronOS.img");
    println!("  -> Created Raw UEFI Image:   {}", img_path.display());

    // 2. Write Fixed VHD (`CelebronOS.vhd`)
    let vhd_footer = build_vhd_footer(DISK_SIZE_BYTES as u64);
    let mut vhd_disk = raw_disk.clone();
    vhd_disk.extend_from_slice(&vhd_footer);
    let vhd_path = out_dir.join("CelebronOS.vhd");
    fs::write(&vhd_path, &vhd_disk).expect("Failed to write CelebronOS.vhd");
    println!("  -> Created Fixed VHD Image:  {}", vhd_path.display());

    // 3. Write Fixed VHDX (`CelebronOS.vhdx`) for Windows Hyper-V Generation 2 (UEFI)
    let vhdx_disk = build_vhdx_image(&raw_disk);
    let vhdx_path = out_dir.join("CelebronOS.vhdx");
    fs::write(&vhdx_path, &vhdx_disk).expect("Failed to write CelebronOS.vhdx");
    println!("  -> Created Hyper-V VHDX:     {}", vhdx_path.display());

    verify_with_virtdisk(&vhdx_path, 3); // 3 = VIRTUAL_STORAGE_TYPE_DEVICE_VHDX
    verify_with_virtdisk(&vhd_path, 2);  // 2 = VIRTUAL_STORAGE_TYPE_DEVICE_VHD
}

#[repr(C)]
struct VirtualStorageType {
    device_id: u32,
    vendor_id: [u8; 16],
}

#[link(name = "virtdisk")]
extern "system" {
    fn OpenVirtualDisk(
        virtual_storage_type: *const VirtualStorageType,
        path: *const u16,
        virtual_disk_access_mask: u32,
        flags: u32,
        parameters: *const core::ffi::c_void,
        handle: *mut isize,
    ) -> u32;

    fn GetVirtualDiskInformation(
        virtual_disk_handle: isize,
        virtual_disk_info_size: *mut u32,
        virtual_disk_info: *mut u8,
        size_used: *mut u32,
    ) -> u32;

    fn CloseHandle(handle: isize) -> i32;
}

fn verify_with_virtdisk(path: &std::path::Path, device_id: u32) {
    use std::os::windows::ffi::OsStrExt;
    let wide_path: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // MICROSOFT_VENDOR_GUID: EC984AEC-A0F9-47e9-901F-71415A66345B
    let vst = VirtualStorageType {
        device_id,
        vendor_id: [
            0xEC, 0x4A, 0x98, 0xEC, 0xF9, 0xA0, 0xE9, 0x47,
            0x90, 0x1F, 0x71, 0x41, 0x5A, 0x66, 0x34, 0x5B,
        ],
    };
    let mut handle: isize = 0;
    // VIRTUAL_DISK_ACCESS_GET_INFO = 0x00080000
    let status = unsafe {
        OpenVirtualDisk(
            &vst,
            wide_path.as_ptr(),
            0x0008_0000,
            0,
            std::ptr::null(),
            &mut handle,
        )
    };
    let mut info_buf = [0u8; 64];
    info_buf[0..4].copy_from_slice(&1u32.to_le_bytes()); // Version = GET_VIRTUAL_DISK_INFO_SIZE (1)
    let mut info_size = info_buf.len() as u32;
    let mut size_used = 0u32;
    let info_status = if status == 0 {
        unsafe {
            let s = GetVirtualDiskInformation(handle, &mut info_size, info_buf.as_mut_ptr(), &mut size_used);
            CloseHandle(handle);
            s
        }
    } else {
        status
    };
    println!(
        "  [VirtDisk.dll Verification] {} -> Open={} GetInfo={} (0 = ERROR_SUCCESS)",
        path.file_name().unwrap().to_string_lossy(),
        status,
        info_status
    );
}
