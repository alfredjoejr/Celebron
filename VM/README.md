# Celebron OS — Virtual Machine Disk Images (`VM/`)

This folder contains the proprietary disk image packager (`disk-builder`) and the compiled bootable disk images for **Celebron OS v0.1**:

- `CelebronOS.vhdx` — **Windows Hyper-V (Generation 2 UEFI)** virtual hard disk (GPT + FAT32 EFI System Partition).
- `CelebronOS.vhd` — Fixed Microsoft VHD image (VirtualBox / legacy VHD tools).
- `CelebronOS.img` — Raw 64 MiB GPT + FAT32 UEFI disk image (QEMU / raw hypervisors).

---

## Rebuilding the Images

Run from PowerShell:
```powershell
powershell -ExecutionPolicy Bypass -File .\build-vm-image.ps1
```

---

## How to Boot `CelebronOS.vhdx` in Windows Hyper-V Manager

1. Open **Hyper-V Manager** → click **New → Virtual Machine...**
2. **Name:** `CelebronOS`
3. **Specify Generation:** Select **Generation 2** (required for 64-bit UEFI boot).
4. **Assign Memory:** `512 MB` (or `1024 MB`), uncheck Dynamic Memory if you want fixed RAM reporting.
5. **Configure Networking:** `Not Connected` (networking arrives in Phase 4).
6. **Connect Virtual Hard Disk:** Select **Use an existing virtual hard disk** and browse to:
   `c:\Users\joash\OneDrive\Documents\Github\Celebron\VM\CelebronOS.vhdx`
7. Click **Finish**, then right-click **`CelebronOS` → Settings...**:
   - Go to **Security** and **uncheck `Enable Secure Boot`** (since our proprietary `BOOTX64.EFI` is not signed by Microsoft's third-party CA).
   - Confirm under **Firmware** that `Hard Drive (CelebronOS.vhdx)` is at the top of the boot order.
8. Double-click **`CelebronOS`** to open the VM window and click **Start**!

### What You Will See on Screen
1. The custom UEFI bootloader (`\EFI\BOOT\BOOTX64.EFI`) runs steps `[1/5]` through `[5/5]`, reads the ACPI RSDP and UEFI GOP framebuffer, loads `\EFI\CELEBRON\KERNEL.ELF`, and calls `ExitBootServices`.
2. The **Celebron OS v0.1 Pure Microkernel (Ring 0)** takes over the display and renders the live telemetry dashboard showing your verified `"CELEBRON"` BootInfo signature, GOP resolution and physical framebuffer address, ACPI 2.0 RSDP address, and usable physical RAM in MiB.
