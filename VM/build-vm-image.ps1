# Celebron OS — Build Bootable Hyper-V / UEFI Virtual Disk Images
$ErrorActionPreference = "Stop"
$VmRoot = $PSScriptRoot
$RepoRoot = Split-Path -Parent $VmRoot
$KernelRoot = Join-Path $RepoRoot "Kernel"

# 1. Compile Microkernel & UEFI Bootloader
powershell -ExecutionPolicy Bypass -File (Join-Path $KernelRoot "build-esp.ps1")

$BootloaderEfi = Join-Path $KernelRoot "build\esp\EFI\BOOT\BOOTX64.EFI"
$KernelElf = Join-Path $KernelRoot "build\esp\EFI\CELEBRON\KERNEL.ELF"

# 2. Run the Celebron Disk Builder to generate CelebronOS.vhdx, CelebronOS.vhd, and CelebronOS.img
Write-Host "`n[4/4] Building Bootable GPT + FAT32 Virtual Disk Images in $VmRoot..." -ForegroundColor Cyan
Set-Location (Join-Path $VmRoot "disk-builder")
cargo run --release -- $BootloaderEfi $KernelElf $VmRoot

Write-Host "`nDone! Your bootable Celebron OS images are ready in: $VmRoot" -ForegroundColor Green
