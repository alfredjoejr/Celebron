//! Celebron OS Ring-3 Userspace Task Loader & Initialization.
//! Maps isolated lower-half user pages (`PAGE_USER`) for Code, Stack, and Shared Memory,
//! copies the initial freestanding Ring-3 `uinit-task` executable payload, seeds task capabilities,
//! and spawns the first preemptively scheduled userspace thread (TID 3).

use crate::ipc::SharedRingBuffer;
use crate::pmm::{PhysicalMemoryManager, PAGE_SIZE};
use crate::scheduler::{self, ThreadPriority};
use crate::vmm::{VirtualMemoryManager, PAGE_PRESENT, PAGE_USER, PAGE_WRITABLE};
use core::arch::global_asm;

pub const USER_CODE_VIRT_ADDR: u64 = 0x0000_0000_0040_0000;
pub const USER_STACK_VIRT_TOP: u64 = 0x0000_0000_0080_0000;
pub const USER_STACK_PAGES: usize = 4; // 16 KiB user stack
pub const USER_SHARED_MEM_VIRT: u64 = 0x0000_0000_00A0_0000;

extern "C" {
    fn user_init_code_start();
    fn user_init_code_end();
}

global_asm!(
    r#"
.section .user_init, "ax"
.global user_init_code_start
.global user_init_code_end

user_init_code_start:
    # 1. SYS_PRINT: Display announcement via capability handle 1
    # ABI: rax=2, rdi=1 (CAP_SYS_CONSOLE), rsi=msg_ptr, rdx=len
    lea rsi, [rip + user_banner_msg]
    mov rdx, 36
    mov rdi, 1
    mov rax, 2
    syscall

    # 2. SYS_IPC_SEND: Fast synchronous register-IPC on capability handle 3
    # ABI: rax=6, rdi=3 (CAP_IPC_ENDPOINT_INIT), rsi=0x1337 (label), rdx=0xCAFE, r10=0xBEEF
    mov rdi, 3
    mov rsi, 0x1337
    mov rdx, 0xCAFE
    mov r10, 0xBEEF
    mov r8, 0x2026
    mov rax, 6
    syscall

    # 3. Main execution loop: yield to scheduler and execute periodic work
1:
    mov rax, 1               # SYS_YIELD
    syscall

    # Delay spin in Ring 3
    mov rcx, 500000
2:
    pause
    dec rcx
    jnz 2b
    jmp 1b

user_banner_msg:
    .ascii "CELEBRON RING-3 INIT TASK ONLINE OK\n"
user_init_code_end:
"#
);

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct UserspaceInfo {
    pub tid: usize,
    pub code_vaddr: u64,
    pub stack_vaddr_top: u64,
    pub shared_mem_vaddr: u64,
    pub code_size_bytes: usize,
}

/// Set up virtual memory mappings for userspace, copy the user executable payload,
/// initialize the shared memory ring buffer, and spawn TID 3 (`uinit-task`).
pub fn init(pmm: &mut PhysicalMemoryManager, vmm: &mut VirtualMemoryManager) -> UserspaceInfo {
    let user_flags = PAGE_PRESENT | PAGE_WRITABLE | PAGE_USER;

    unsafe {
        // 1. Allocate and map User Code page at 0x0000_0000_0040_0000
        let code_frame = pmm
            .alloc_frame()
            .expect("Out of physical memory allocating user code page");
        vmm.map_4k(USER_CODE_VIRT_ADDR, code_frame, user_flags, pmm);

        // 2. Allocate and map 16 KiB User Stack (4 pages ending at 0x0000_0000_0080_0000)
        let stack_start = USER_STACK_VIRT_TOP - (USER_STACK_PAGES as u64 * PAGE_SIZE);
        for i in 0..USER_STACK_PAGES {
            let stack_frame = pmm
                .alloc_frame()
                .expect("Out of physical memory allocating user stack page");
            let vaddr = stack_start + (i as u64 * PAGE_SIZE);
            vmm.map_4k(vaddr, stack_frame, user_flags, pmm);
        }

        // 3. Allocate and map Shared Memory Ring Buffer at 0x0000_0000_00A0_0000
        let shared_frame = pmm
            .alloc_frame()
            .expect("Out of physical memory allocating user shared memory page");
        vmm.map_4k(USER_SHARED_MEM_VIRT, shared_frame, user_flags, pmm);

        // Initialize Shared Ring Buffer header in the shared page
        let ring_ptr = USER_SHARED_MEM_VIRT as *mut SharedRingBuffer;
        core::ptr::write(ring_ptr, SharedRingBuffer::new());

        // 4. Copy freestanding Ring-3 executable code to USER_CODE_VIRT_ADDR
        let src_start = user_init_code_start as *const () as usize;
        let src_end = user_init_code_end as *const () as usize;
        let code_len = src_end.saturating_sub(src_start);
        assert!(code_len > 0 && code_len <= PAGE_SIZE as usize, "Invalid user init code size");

        let src_slice = core::slice::from_raw_parts(src_start as *const u8, code_len);
        let dst_slice = core::slice::from_raw_parts_mut(USER_CODE_VIRT_ADDR as *mut u8, code_len);
        dst_slice.copy_from_slice(src_slice);

        crate::kprintln!(
            "[CELEBRON-USER] Mapped Ring-3 code ({} bytes @ 0x{:016X}), stack (16 KiB @ 0x{:016X}), shared ring (0x{:016X})",
            code_len,
            USER_CODE_VIRT_ADDR,
            USER_STACK_VIRT_TOP,
            USER_SHARED_MEM_VIRT
        );

        // 5. Spawn Ring-3 Userspace Task (TID 3)
        // System V AMD64 ABI requires initial RSP % 16 == 8 or 0 before entry.
        let user_initial_rsp = USER_STACK_VIRT_TOP - 16;
        let tid = scheduler::spawn_user_thread(
            "uinit-task",
            ThreadPriority::Normal,
            USER_CODE_VIRT_ADDR,
            user_initial_rsp,
        );

        UserspaceInfo {
            tid,
            code_vaddr: USER_CODE_VIRT_ADDR,
            stack_vaddr_top: USER_STACK_VIRT_TOP,
            shared_mem_vaddr: USER_SHARED_MEM_VIRT,
            code_size_bytes: code_len,
        }
    }
}
