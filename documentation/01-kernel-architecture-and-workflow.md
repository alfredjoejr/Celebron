# Celebron Kernel — Architecture & Development Workflow

> **Phase:** Phase 1 — Custom UEFI Bootloader & Pure Microkernel Foundation  
> **Stack:** Rust (`no_std`) + minimal `x86_64` Assembly | Target: `x86_64` | Model: Pure Capability-Based Microkernel  
> **Goal:** Design and build a minimal, proprietary, bootable 64-bit pure microkernel and custom UEFI bootloader capable of hosting isolated userspace drivers, the custom file system, and the Cognitive Core.

---

## 1. Kernel Design Principles

1. **Minimal Trusted Computing Base (Ring 0):** Keep the privileged kernel small, auditable, and crash-resistant.
2. **Capability-Based Security:** Every kernel object (memory region, IPC channel, IRQ, I/O port, process) is referenced via an unforgeable capability handle. No ambient "root" privileges for apps or AI agents.
3. **Userspace Driver Isolation:** Hardware drivers run in sandboxed Ring 3 processes communicating over fast IPC. A faulty third-party driver restarts cleanly without crashing the OS.
4. **Clean, Versioned ABI:** Establish a strict syscall and IPC contract early so userspace services and third-party drivers remain decoupled from internal kernel changes.
5. **Zero Copyleft Contamination:** All kernel code is original proprietary code; any third-party helper crates/libraries must use permissive licenses (MIT, Apache-2.0, BSD, Zlib, 0BSD).

---

## 2. Core Kernel Subsystems (Ring 0)

| Subsystem | Responsibility |
| :--- | :--- |
| **Boot & Hardware Hand-off** | Parse UEFI memory map, ACPI tables, and framebuffer info; transition to higher-half virtual address space. |
| **Physical Memory Manager (PMM)** | Track free/used 4 KiB physical frames (bitmap or buddy allocator). |
| **Virtual Memory Manager (VMM)** | Manage 4-level/5-level page tables, kernel heap, and isolated per-process address spaces. |
| **Interrupt & Exception Handling** | IDT/GDT/TSS setup, APIC/IOAPIC routing, CPU exception handlers, and timer ticks. |
| **Scheduler & Multitasking** | Preemptive, priority-aware thread scheduling (SMP multi-core ready). |
| **Capability Table & Fast IPC** | Per-process capability tables and synchronous/asynchronous message passing between services. |
| **Syscall Interface** | Fast `syscall`/`sysret` entry point validating capability handles and dispatching kernel requests. |

---

## 3. Kernel Development Workflow

### Daily Build-Boot-Test Loop
To iterate rapidly on bare-metal code without rebooting physical hardware:

```text
Write Kernel Code
       │
       ▼
Cross-Compile Kernel Binary (Freestanding, No Standard OS Libs)
       │
       ▼
Package Bootable UEFI Disk Image (.img / .iso with OVMF firmware)
       │
       ▼
Boot in QEMU (with Serial Console + GDB Stub Attached)
       │
       ▼
Run Automated Kernel Self-Tests & Inspect Serial Logs
```

### Verification & Tooling Practices
- **Emulation:** QEMU (`qemu-system-x86_64`) with OVMF UEFI firmware for repeatable boots.
- **Debugging:** Serial port (`COM1`) structured logging + GDB remote debugging (`-s -S`) for stepping through early boot and page faults.
- **Automated Testing:** Custom kernel test runner that boots QEMU, executes unit/integration tests (PMM, VMM, IPC, scheduler), and exits QEMU via an I/O debug exit port with pass/fail status.
- **Documentation First:** Every syscall, memory layout, and IPC protocol is specified in `documentation/` before implementation.

---

## 4. Kernel Implementation Milestones

### Milestone 1.1 — Custom UEFI Bootloader & Kernel Observability
- [x] Set up Rust workspace with `x86_64-unknown-uefi` (bootloader) and `x86_64-unknown-none` (kernel) targets.
- [x] Build custom UEFI bootloader: query GOP framebuffer, read UEFI memory map, locate ACPI RSDP, load kernel ELF into memory, exit UEFI Boot Services, and jump to higher-half/Ring-0 kernel entry.
- [x] Initialize serial logging (`COM1`) and UEFI GOP framebuffer output in the kernel.
- [x] Implement kernel panic handler.

### Milestone 1.2 — Memory Management
- [ ] Parse UEFI memory map and build the Physical Memory Manager (PMM).
- [ ] Set up higher-half kernel paging and Virtual Memory Manager (VMM).
- [ ] Implement kernel heap allocator (`alloc` support).

### Milestone 1.3 — Interrupts & Timing
- [ ] Configure GDT, TSS, and IDT (CPU exceptions + double-fault stack).
- [ ] Initialize Local APIC, IOAPIC, and hardware timer (HPET / APIC timer).
- [ ] Verify clean interrupt handling and timer ticks.

### Milestone 1.4 — Multitasking & Scheduling
- [ ] Implement kernel threads and context switching.
- [ ] Build preemptive round-robin / priority scheduler.
- [ ] Add SMP (multi-core CPU) bring-up support.

### Milestone 1.5 — Userspace, Capabilities & IPC
- [ ] Implement `syscall`/`sysret` transition to Ring 3 (userspace).
- [ ] Build capability handle system (grant, revoke, restrict rights).
- [ ] Implement fast IPC endpoints and shared-memory ring buffers.
- [ ] Load and execute the first userspace `init` process (ELF or custom binary format).

### Milestone 1.6 — Driver Foundation (Bridge to Phase 2)
- [ ] Implement PCIe bus enumeration and ACPI parsing.
- [ ] Expose capability-gated MMIO / DMA / IRQ primitives to userspace driver processes.
- [ ] Build first userspace VirtIO-Blk / NVMe driver to unlock Phase 2 (Custom File System).

---

## 5. Next Phases (Preview)

Once Milestone 1.6 is complete, Celebron moves into:
- **Phase 2:** Custom Copy-on-Write + Semantic File System Server.
- **Phase 3:** Display Compositor, Input Stack, and Desktop Shell.
- **Phase 4:** Networking Stack & Local AI Runtime ("Cognitive Core" + Intent Bus).
- **Phase 5:** Third-Party Driver Kit (DK) & Application SDK.
