# Celebron OS — Build & Stage UEFI System Partition (ESP)
$ErrorActionPreference = "Stop"
$KernelRoot = $PSScriptRoot
Set-Location $KernelRoot

$EspBootDir = Join-Path $KernelRoot "build\esp\EFI\BOOT"
$EspCelebronDir = Join-Path $KernelRoot "build\esp\EFI\CELEBRON"

New-Item -ItemType Directory -Force -Path $EspBootDir | Out-Null
New-Item -ItemType Directory -Force -Path $EspCelebronDir | Out-Null

Write-Host "[1/3] Building Celebron Pure Microkernel (x86_64-unknown-none)..." -ForegroundColor Cyan
cargo build --release -p celebron-kernel --target x86_64-unknown-none

$KernelElf = Join-Path $KernelRoot "target\x86_64-unknown-none\release\celebron-kernel"
Copy-Item $KernelElf (Join-Path $EspCelebronDir "KERNEL.ELF") -Force

Write-Host "[2/3] Building Celebron Proprietary UEFI Bootloader (x86_64-unknown-uefi)..." -ForegroundColor Cyan
cargo build --release -p celebron-bootloader --target x86_64-unknown-uefi

$BootloaderEfi = Join-Path $KernelRoot "target\x86_64-unknown-uefi\release\celebron-bootloader.efi"
Copy-Item $BootloaderEfi (Join-Path $EspBootDir "BOOTX64.EFI") -Force

Write-Host "[3/3] Staged UEFI System Partition at: $KernelRoot\build\esp" -ForegroundColor Green
Write-Host "      -> EFI\BOOT\BOOTX64.EFI"
Write-Host "      -> EFI\CELEBRON\KERNEL.ELF"
