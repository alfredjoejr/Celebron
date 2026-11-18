# Celebron OS

> **Proprietary & Confidential — All Rights Reserved.**  
> Celebron OS is an independent, proprietary 64-bit AI-native operating system built on a custom Rust capability-based pure microkernel and custom UEFI bootloader.

---

## Repository Structure

- [`documentation/`](./documentation/) — Vision, locked-in architectural decisions, and phase workflows:
  - [`00-vision-and-approved-decisions.md`](./documentation/00-vision-and-approved-decisions.md)
  - [`01-kernel-architecture-and-workflow.md`](./documentation/01-kernel-architecture-and-workflow.md)
- [`Kernel/`](./Kernel/) — Celebron OS Bare-Metal Workspace:
  - [`boot-info/`](./Kernel/boot-info/) — Proprietary Boot Hand-off Protocol ABI (`BootInfo`, `FramebufferInfo`, `MemoryRegion`).
  - [`bootloader/`](./Kernel/bootloader/) — Proprietary Custom UEFI Bootloader (`x86_64-unknown-uefi`, `\EFI\BOOT\BOOTX64.EFI`).
  - [`kernel/`](./Kernel/kernel/) — Celebron Pure Microkernel (`x86_64-unknown-none`, Ring 0, `\EFI\CELEBRON\KERNEL.ELF`).
- [`VM/`](./VM/) — Virtual Hard Disk Image Builder (`disk-builder`) & Hyper-V / UEFI boot scripts:
  - [`build-vm-image.ps1`](./VM/build-vm-image.ps1) — Builds `CelebronOS.vhdx` (Hyper-V Gen 2), `CelebronOS.vhd`, and `CelebronOS.img`.

---

## Branch Workflow

- `main` — Stable, verified milestone releases.
- `dev` — Active development branch (all ongoing work is committed and pushed here).
