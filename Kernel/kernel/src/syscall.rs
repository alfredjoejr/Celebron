//! Celebron OS Fast System Call Subsystem (`syscall` / `sysret`).
//! Configures x86_64 Long-Mode MSRs (`STAR`, `LSTAR`, `FMASK`, `EFER`), safely transitions
//! stacks across the privilege boundary using `swapgs`, verifies unforgeable capability handles,
//! and dispatches userspace microkernel requests.

use crate::capability::{
    CapError, CapRights, CapType, Capability, CapabilityTable, CAP_DRV_DMA, CAP_DRV_IRQ,
    CAP_DRV_MMIO, CAP_IPC_ENDPOINT_INIT, CAP_MEM_SHARED, CAP_SYS_CONSOLE, CAP_SYS_INFO,
    CAP_SYS_POWER,
};
use crate::gdt::KERNEL_CODE_SELECTOR;
use crate::interrupts;
use crate::ipc::{self, IpcError, IpcMessage};
use crate::power::{self, PowerAction};
use core::arch::{asm, global_asm};
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicU64, Ordering};

pub const SYS_YIELD: u64 = 1;
pub const SYS_PRINT: u64 = 2;
pub const SYS_CAP_INFO: u64 = 3;
pub const SYS_CAP_DERIVE: u64 = 4;
pub const SYS_CAP_REVOKE: u64 = 5;
pub const SYS_IPC_SEND: u64 = 6;
pub const SYS_IPC_RECV: u64 = 7;
pub const SYS_GET_TICKS: u64 = 9;
pub const SYS_POWER_CONTROL: u64 = 10;
pub const SYS_DRIVER_MMIO_READ: u64 = 11;
pub const SYS_DRIVER_MMIO_WRITE: u64 = 12;
pub const SYS_DRIVER_DMA_INFO: u64 = 13;
pub const SYS_DRIVER_IRQ_ACK: u64 = 14;

pub const RET_SUCCESS: u64 = 0;
pub const ERR_INVALID_SYSCALL: u64 = 1;
pub const ERR_CAP_INVALID: u64 = 2;
pub const ERR_CAP_PERM_DENIED: u64 = 3;
pub const ERR_INVALID_ARG: u64 = 4;
pub const ERR_IPC_CHANNEL_FULL: u64 = 5;
pub const ERR_IPC_NO_MESSAGE: u64 = 6;

const IA32_EFER_MSR: u32 = 0xC000_0080;
const IA32_STAR_MSR: u32 = 0xC000_0081;
const IA32_LSTAR_MSR: u32 = 0xC000_0082;
const IA32_FMASK_MSR: u32 = 0xC000_0084;
const IA32_KERNEL_GS_BASE_MSR: u32 = 0xC000_0102;

const SYSCALL_STACK_SIZE: usize = 32 * 1024;

#[repr(C, align(16))]
struct SyscallStack([u8; SYSCALL_STACK_SIZE]);
static mut SYSCALL_KERNEL_STACK: SyscallStack = SyscallStack([0; SYSCALL_STACK_SIZE]);

/// Kernel Per-CPU Context pointed to by `IA32_KERNEL_GS_BASE`.
/// Accessed via `gs:[0]` and `gs:[8]` immediately after `swapgs` in `syscall_entry_stub`.
#[repr(C, align(16))]
pub struct SyscallCpuContext {
    pub kernel_rsp: u64, // offset 0
    pub user_rsp: u64,   // offset 8
}

static mut SYSCALL_CONTEXT: SyscallCpuContext = SyscallCpuContext {
    kernel_rsp: 0,
    user_rsp: 0,
};

static mut ACTIVE_CAP_TABLE: CapabilityTable = CapabilityTable::new();

static SYSCALL_TOTAL_COUNT: AtomicU64 = AtomicU64::new(0);
static SYSCALL_YIELD_COUNT: AtomicU64 = AtomicU64::new(0);
static SYSCALL_PRINT_COUNT: AtomicU64 = AtomicU64::new(0);
static SYSCALL_IPC_COUNT: AtomicU64 = AtomicU64::new(0);

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct SyscallStats {
    pub total_syscalls: u64,
    pub yield_count: u64,
    pub print_count: u64,
    pub ipc_count: u64,
}

pub fn stats() -> SyscallStats {
    SyscallStats {
        total_syscalls: SYSCALL_TOTAL_COUNT.load(Ordering::Relaxed),
        yield_count: SYSCALL_YIELD_COUNT.load(Ordering::Relaxed),
        print_count: SYSCALL_PRINT_COUNT.load(Ordering::Relaxed),
        ipc_count: SYSCALL_IPC_COUNT.load(Ordering::Relaxed),
    }
}

unsafe fn rdmsr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    asm!(
        "rdmsr",
        in("ecx") msr,
        out("eax") low,
        out("edx") high,
        options(nomem, nostack, preserves_flags)
    );
    ((high as u64) << 32) | (low as u64)
}

unsafe fn wrmsr(msr: u32, val: u64) {
    let low = (val & 0xFFFF_FFFF) as u32;
    let high = ((val >> 32) & 0xFFFF_FFFF) as u32;
    asm!(
        "wrmsr",
        in("ecx") msr,
        in("eax") low,
        in("edx") high,
        options(nomem, nostack, preserves_flags)
    );
}

extern "C" {
    fn syscall_entry_stub();
}

global_asm!(
    r#"
.global syscall_entry_stub
.type syscall_entry_stub, @function
syscall_entry_stub:
    # 1. Swap user GS base with kernel GS base
    swapgs
    mov gs:[8], rsp          # Save user stack pointer
    mov rsp, gs:[0]          # Switch to safe Ring-0 syscall stack

    # 2. Preserve caller-saved and syscall link registers
    # Note: System V ABI requires 16-byte RSP alignment before call.
    # We push 8 registers = 64 bytes (preserves alignment: 64 % 16 == 0).
    push rcx                 # User return RIP (saved by CPU syscall)
    push r11                 # User return RFLAGS (saved by CPU syscall)
    push rbp
    push rbx
    push r12
    push r13
    push r14
    push r15

    # 3. Rearrange registers for SysV C calling convention:
    # rust_syscall_dispatch(num, a1, a2, a3, a4, a5)
    # ABI target:  rdi, rsi, rdx, rcx, r8, r9
    # User input:  rax=num, rdi=a1, rsi=a2, rdx=a3, r10=a4, r8=a5
    mov r9, r8               # a5
    mov r8, r10              # a4
    mov rcx, rdx             # a3
    mov rdx, rsi             # a2
    mov rsi, rdi             # a1
    mov rdi, rax             # syscall number

    call rust_syscall_dispatch
    # Result returned in RAX

    # 4. Restore preserved registers
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbx
    pop rbp
    pop r11                  # Restore return RFLAGS
    pop rcx                  # Restore return RIP

    # 5. Restore user stack and swap GS back to user mode
    mov rsp, gs:[8]
    swapgs
    sysretq
"#
);

/// Ring-0 Syscall Dispatcher called directly from `syscall_entry_stub`.
#[no_mangle]
pub extern "sysv64" fn rust_syscall_dispatch(
    num: u64,
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    _a5: u64,
) -> u64 {
    SYSCALL_TOTAL_COUNT.fetch_add(1, Ordering::Relaxed);

    let table = unsafe { &mut *addr_of_mut!(ACTIVE_CAP_TABLE) };

    match num {
        SYS_YIELD => {
            SYSCALL_YIELD_COUNT.fetch_add(1, Ordering::Relaxed);
            RET_SUCCESS
        }

        SYS_PRINT => {
            let cap_handle = a1 as u32;
            let str_ptr = a2 as *const u8;
            let len = a3 as usize;

            if str_ptr.is_null() || len == 0 || len > 256 {
                return ERR_INVALID_ARG;
            }

            match table.verify(cap_handle, CapRights::WRITE) {
                Ok(cap) => {
                    if cap.cap_type != CapType::System {
                        return ERR_CAP_PERM_DENIED;
                    }
                    // Validate string pointer is in canonical lower half (userspace)
                    if (str_ptr as u64) >= 0x0000_8000_0000_0000 {
                        return ERR_INVALID_ARG;
                    }

                    let bytes = unsafe { core::slice::from_raw_parts(str_ptr, len) };
                    if let Ok(msg) = core::str::from_utf8(bytes) {
                        crate::kprint!("[USER-RING3] {}", msg);
                    }
                    SYSCALL_PRINT_COUNT.fetch_add(1, Ordering::Relaxed);
                    RET_SUCCESS
                }
                Err(CapError::PermissionDenied) => ERR_CAP_PERM_DENIED,
                Err(_) => ERR_CAP_INVALID,
            }
        }

        SYS_CAP_INFO => {
            let cap_handle = a1 as u32;
            match table.get(cap_handle) {
                Ok(cap) => ((cap.rights as u64) << 32) | (cap.cap_type as u64),
                Err(_) => ERR_CAP_INVALID,
            }
        }

        SYS_CAP_DERIVE => {
            let cap_handle = a1 as u32;
            let rights = a2 as u32;
            match table.derive(cap_handle, rights) {
                Ok(new_handle) => new_handle as u64,
                Err(CapError::PermissionDenied) => ERR_CAP_PERM_DENIED,
                Err(_) => ERR_CAP_INVALID,
            }
        }

        SYS_CAP_REVOKE => {
            let cap_handle = a1 as u32;
            match table.revoke(cap_handle) {
                Ok(()) => RET_SUCCESS,
                Err(_) => ERR_CAP_INVALID,
            }
        }

        SYS_IPC_SEND => {
            let cap_handle = a1 as u32;
            let label = a2;
            let d0 = a3;
            let d1 = a4;

            match table.verify(cap_handle, CapRights::SEND) {
                Ok(cap) => {
                    if cap.cap_type != CapType::IpcEndpoint {
                        return ERR_CAP_PERM_DENIED;
                    }
                    let msg = IpcMessage::new(cap.badge, label, d0, d1, 0, 0);
                    match ipc::channel_send(cap.object_id, msg) {
                        Ok(()) => {
                            SYSCALL_IPC_COUNT.fetch_add(1, Ordering::Relaxed);
                            RET_SUCCESS
                        }
                        Err(IpcError::ChannelFull) => ERR_IPC_CHANNEL_FULL,
                        Err(_) => ERR_INVALID_ARG,
                    }
                }
                Err(CapError::PermissionDenied) => ERR_CAP_PERM_DENIED,
                Err(_) => ERR_CAP_INVALID,
            }
        }

        SYS_IPC_RECV => {
            let cap_handle = a1 as u32;
            let out_ptr = a2 as *mut IpcMessage;

            if out_ptr.is_null() || (out_ptr as u64) >= 0x0000_8000_0000_0000 {
                return ERR_INVALID_ARG;
            }

            match table.verify(cap_handle, CapRights::RECV) {
                Ok(cap) => {
                    if cap.cap_type != CapType::IpcEndpoint {
                        return ERR_CAP_PERM_DENIED;
                    }
                    match ipc::channel_recv(cap.object_id) {
                        Ok(msg) => {
                            unsafe {
                                *out_ptr = msg;
                            }
                            SYSCALL_IPC_COUNT.fetch_add(1, Ordering::Relaxed);
                            RET_SUCCESS
                        }
                        Err(IpcError::NoMessageAvailable) => ERR_IPC_NO_MESSAGE,
                        Err(_) => ERR_INVALID_ARG,
                    }
                }
                Err(CapError::PermissionDenied) => ERR_CAP_PERM_DENIED,
                Err(_) => ERR_CAP_INVALID,
            }
        }

        SYS_GET_TICKS => interrupts::timer_ticks(),

        SYS_POWER_CONTROL => {
            let cap_handle = a1 as u32;
            let action = match PowerAction::from_u64(a2) {
                Some(act) => act,
                None => return ERR_INVALID_ARG,
            };
            match table.verify(cap_handle, CapRights::WRITE) {
                Ok(cap) => {
                    if cap.cap_type != CapType::System || cap.object_id != 5 {
                        return ERR_CAP_PERM_DENIED;
                    }
                    power::signal_power_action(action);
                    RET_SUCCESS
                }
                Err(CapError::PermissionDenied) => ERR_CAP_PERM_DENIED,
                Err(_) => ERR_CAP_INVALID,
            }
        }

        SYS_DRIVER_MMIO_READ => {
            let cap_handle = a1 as u32;
            let offset = a2;
            if offset >= 4096 || (offset & 0x3) != 0 {
                return ERR_INVALID_ARG;
            }
            match table.verify(cap_handle, CapRights::READ) {
                Ok(cap) => {
                    if cap.cap_type != CapType::DeviceMmio || cap.object_id == 0 {
                        return ERR_CAP_PERM_DENIED;
                    }
                    unsafe { core::ptr::read_volatile((cap.object_id + offset) as *const u32) as u64 }
                }
                Err(CapError::PermissionDenied) => ERR_CAP_PERM_DENIED,
                Err(_) => ERR_CAP_INVALID,
            }
        }

        SYS_DRIVER_MMIO_WRITE => {
            let cap_handle = a1 as u32;
            let offset = a2;
            let val = a3 as u32;
            if offset >= 4096 || (offset & 0x3) != 0 {
                return ERR_INVALID_ARG;
            }
            match table.verify(cap_handle, CapRights::WRITE) {
                Ok(cap) => {
                    if cap.cap_type != CapType::DeviceMmio || cap.object_id == 0 {
                        return ERR_CAP_PERM_DENIED;
                    }
                    unsafe {
                        core::ptr::write_volatile((cap.object_id + offset) as *mut u32, val);
                    }
                    RET_SUCCESS
                }
                Err(CapError::PermissionDenied) => ERR_CAP_PERM_DENIED,
                Err(_) => ERR_CAP_INVALID,
            }
        }

        SYS_DRIVER_DMA_INFO => {
            let cap_handle = a1 as u32;
            match table.verify(cap_handle, CapRights::READ) {
                Ok(cap) => {
                    if cap.cap_type != CapType::DmaBuffer {
                        return ERR_CAP_PERM_DENIED;
                    }
                    cap.object_id
                }
                Err(CapError::PermissionDenied) => ERR_CAP_PERM_DENIED,
                Err(_) => ERR_CAP_INVALID,
            }
        }

        SYS_DRIVER_IRQ_ACK => {
            let cap_handle = a1 as u32;
            match table.verify(cap_handle, CapRights::WRITE) {
                Ok(cap) => {
                    if cap.cap_type != CapType::Irq {
                        return ERR_CAP_PERM_DENIED;
                    }
                    RET_SUCCESS
                }
                Err(CapError::PermissionDenied) => ERR_CAP_PERM_DENIED,
                Err(_) => ERR_CAP_INVALID,
            }
        }

        _ => ERR_INVALID_SYSCALL,
    }
}

/// Grant capability-gated MMIO, DMA, and IRQ handles to userspace driver tasks.
pub fn grant_driver_capabilities(mmio_base: u64, dma_phys_addr: u64, irq_line: u8) {
    unsafe {
        let table = &mut *addr_of_mut!(ACTIVE_CAP_TABLE);

        let mmio_cap = Capability::new(
            CapType::DeviceMmio,
            CapRights::READ | CapRights::WRITE | CapRights::GRANT,
            mmio_base,
            0xD001,
        );
        let _ = table.insert_at(CAP_DRV_MMIO as usize, mmio_cap);

        let dma_cap = Capability::new(
            CapType::DmaBuffer,
            CapRights::READ | CapRights::WRITE | CapRights::GRANT,
            dma_phys_addr,
            0xD002,
        );
        let _ = table.insert_at(CAP_DRV_DMA as usize, dma_cap);

        let irq_cap = Capability::new(
            CapType::Irq,
            CapRights::READ | CapRights::WRITE | CapRights::GRANT,
            irq_line as u64,
            0xD003,
        );
        let _ = table.insert_at(CAP_DRV_IRQ as usize, irq_cap);
    }
}

/// Initialize the 64-bit `syscall` and `sysret` MSRs and populate initial user capabilities.
pub fn init() {
    unsafe {
        // 1. Prepare dedicated Ring-0 Syscall Stack & Context
        let stack_top =
            (addr_of!(SYSCALL_KERNEL_STACK.0) as u64 + SYSCALL_STACK_SIZE as u64) & !0xFu64;
        let ctx_ptr = addr_of_mut!(SYSCALL_CONTEXT);
        (*ctx_ptr).kernel_rsp = stack_top;
        (*ctx_ptr).user_rsp = 0;

        // 2. Set IA32_KERNEL_GS_BASE to &SYSCALL_CONTEXT
        wrmsr(IA32_KERNEL_GS_BASE_MSR, ctx_ptr as u64);

        // 3. Enable System Call Extensions (SCE) in IA32_EFER
        let efer = rdmsr(IA32_EFER_MSR);
        wrmsr(IA32_EFER_MSR, efer | 1);

        // 4. Configure IA32_STAR
        // Bits 47:32 = Kernel CS (0x08). SYSCALL loads CS=0x08, SS=0x10.
        // Bits 63:48 = User Base (0x10 | 3 = 0x13). SYSRET loads CS=0x23, SS=0x1B.
        let star_val = (0x0013u64 << 48) | ((KERNEL_CODE_SELECTOR as u64) << 32);
        wrmsr(IA32_STAR_MSR, star_val);

        // 5. Configure IA32_LSTAR with syscall_entry_stub address
        let lstar_val = syscall_entry_stub as *const () as usize as u64;
        wrmsr(IA32_LSTAR_MSR, lstar_val);

        // 6. Configure IA32_FMASK: mask IF (0x200), TF (0x100), DF (0x400), IOPL (0x3000), AC, NT
        wrmsr(IA32_FMASK_MSR, 0x0003_F7FD);

        // 7. Seed initial capability table for the userspace `init` process
        let table = &mut *addr_of_mut!(ACTIVE_CAP_TABLE);
        *table = CapabilityTable::new();

        // Slot 1: Console / Telemetry write capability
        let console_cap =
            Capability::new(CapType::System, CapRights::WRITE | CapRights::GRANT, 1, 0);
        let _ = table.insert_at(CAP_SYS_CONSOLE as usize, console_cap);

        // Slot 2: System info / timer ticks capability
        let info_cap = Capability::new(CapType::System, CapRights::READ | CapRights::GRANT, 2, 0);
        let _ = table.insert_at(CAP_SYS_INFO as usize, info_cap);

        // Slot 3: Rendezvous IPC Endpoint with unforgeable sender badge
        let ipc_cap = Capability::new(
            CapType::IpcEndpoint,
            CapRights::SEND | CapRights::RECV | CapRights::GRANT,
            1, // Channel ID 1
            0xCAFE_0001,
        );
        let _ = table.insert_at(CAP_IPC_ENDPOINT_INIT as usize, ipc_cap);

        // Slot 4: Shared Memory Ring Buffer region
        let mem_cap = Capability::new(
            CapType::Memory,
            CapRights::READ | CapRights::WRITE | CapRights::GRANT,
            0x00A0_0000,
            0,
        );
        let _ = table.insert_at(CAP_MEM_SHARED as usize, mem_cap);

        // Slot 5: System Power Control capability (Shutdown / Cold Reboot / Warm Reboot)
        let power_cap = Capability::new(
            CapType::System,
            CapRights::WRITE | CapRights::GRANT,
            5,
            0x0000_9001,
        );
        let _ = table.insert_at(CAP_SYS_POWER as usize, power_cap);
    }
}
