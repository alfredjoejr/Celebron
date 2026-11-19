# Celebron OS — Phase 2: Persistent Storage & CelebronFS Architecture

> **Phase:** Phase 2 — Persistent Block Storage & Copy-on-Write / Semantic File System (`CelebronFS`)
> **Status:** Milestone 2.1 Active
> **Base:** Microkernel Phase 1 (`main` @ `1a19a59`)

---

## 1. Phase 2 Goals

Establish persistent non-volatile block storage on native Hyper-V Generation 2 (VMBus Synthetic SCSI / StorVSC) and compatible hardware, isolate storage partitions via GPT and capability-scoped Ring-0 handles, and build an independent, Copy-on-Write (CoW), snapshot-capable, AI-native semantic file system (`CelebronFS`) running as a capability-gated userspace server.

---

## 2. Disk Layout (96 MiB Dual-Partition GPT)

Total virtual disk size: **96 MiB** (`196,608` × 512-byte sectors).

| Partition | Label | LBA Range | Size | Format / Purpose |
| :--- | :--- | :--- | :--- | :--- |
| **Partition 0** | `Celebron EFI System` | `2,048 .. 130,047` | 62.5 MiB (`128,000` sectors) | FAT32 ESP holding `\EFI\BOOT\BOOTX64.EFI` and `\EFI\CELEBRON\KERNEL.ELF` |
| **Partition 1** | `CelebronFS CoW Root` | `131,072 .. 196,574` | ≈32 MiB (`65,503` sectors / `8,187` × 4 KiB blocks) | Proprietary Copy-on-Write & Semantic File System root |

* **Partition 1 Type GUID:** `CE1EB201-F500-4C4F-A100-CE1EB2012026`
  * Mixed-endian byte representation: `[0x01, 0xB2, 0x1E, 0xCE, 0x00, 0xF5, 0x4F, 0x4C, 0xA1, 0x00, 0xCE, 0x1E, 0xB2, 0x01, 0x20, 0x26]`

---

## 3. CelebronFS Volume Header (Sector 0 of Partition 1, LBA `131,072`)

First 68 bytes of sector `131,072` (little-endian):

| Offset | Size | Field | Value / Semantics |
| :--- | :--- | :--- | :--- |
| `0..16` | 16 B | `magic` | `b"CELEBRONFS_V1\0\0\0"` |
| `16..20` | 4 B | `version` | `1u32` |
| `20..24` | 4 B | `block_size` | `4096u32` (4 KiB blocks) |
| `24..32` | 8 B | `partition_lba` | `131_072u64` |
| `32..40` | 8 B | `total_sectors` | `65_503u64` |
| `40..48` | 8 B | `total_blocks` | `8_187u64` |
| `48..56` | 8 B | `mount_count` | Persistent counter incremented & flushed to disk on every verified boot |
| `56..64` | 8 B | `generation` | `1u64` |
| `64..68` | 4 B | `crc32` | IEEE 802.3 CRC-32 (`0xEDB88320`) over bytes `0..64` |

---

## 4. Capability-Gated Block Syscall ABI (Userspace FS Server Bridge)

Ring-3 tasks never receive raw whole-disk LBA access. Partition access is scoped strictly to Partition 1 (`0 .. total_sectors`) via an unforgeable kernel capability:

* **Capability Type:** `CapType::BlockDevice = 8`
* **Standard Initial Handle:** `CAP_BLK_DEV = 9` (`rights = READ | WRITE | GRANT`, `object_id = partition_sectors`, `badge = 0xB10C`)
* **Syscalls:**
  * `SYS_BLK_INFO` (`15`): `(cap_handle)` → returns partition sector count (requires `CapRights::READ`).
  * `SYS_BLK_READ` (`16`): `(cap_handle, rel_lba, user_buf_ptr, sector_count)` → reads `1..=8` sectors relative to Partition 1 start into a validated userspace buffer (requires `CapRights::READ`).
  * `SYS_BLK_WRITE` (`17`): `(cap_handle, rel_lba, user_buf_ptr, sector_count)` → writes `1..=8` sectors relative to Partition 1 start from a validated userspace buffer (requires `CapRights::WRITE`).

---

## 5. Phase 2 Milestones

* ### Milestone 2.1 — Block Storage Driver, GPT Parser & Persistent Partition Verification
  * [x] Specify Phase 2 disk layout, volume header, and capability-gated block ABI.
  * [x] Implement clean-room Hyper-V VMBus StorVSC Synthetic SCSI driver (`SCSI_INTERFACE_ID`).
  * [x] Upgrade VM image builder to 96 MiB dual-partition GPT with initial CelebronFS volume header.
  * [x] Implement kernel GPT parser, Partition 1 mount-count persistence check (`write` + `read-back`), and partition-scoped block syscalls (`SYS_BLK_INFO`, `SYS_BLK_READ`, `SYS_BLK_WRITE`).
* ### Milestone 2.2 — CelebronFS Copy-on-Write Engine (Userspace Server)
  * [ ] Implement dual-superblock atomic commit ring, extent/block bitmap allocator, and B-tree namespace.
  * [ ] Support instant point-in-time CoW snapshots and rollback in isolated Ring-3 server.
* ### Milestone 2.3 — Semantic & Metadata Index Layer
  * [ ] Add native vector/semantic metadata indexing and OS Intent Bus integration for AI-native retrieval.
