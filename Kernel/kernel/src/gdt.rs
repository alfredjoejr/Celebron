//! Celebron OS 64-bit Global Descriptor Table (GDT) and Task State Segment (TSS).
//! Configures Ring-0/Ring-3 code/data descriptors, a dedicated Ring-0 privilege stack (`RSP0`),
//! and an isolated Double-Fault emergency stack (`IST1`).

use core::arch::asm;
use core::ptr::{addr_of, addr_of_mut};

pub const KERNEL_CODE_SELECTOR: u16 = 0x08;
pub const KERNEL_DATA_SELECTOR: u16 = 0x10;
#[allow(dead_code)]
pub const USER_DATA_SELECTOR: u16 = 0x18 | 3;
#[allow(dead_code)]
pub const USER_CODE_SELECTOR: u16 = 0x20 | 3;
pub const TSS_SELECTOR: u16 = 0x28;

/// 1-based IST index used by the IDT `#DF` (Vector 8) gate.
pub const DOUBLE_FAULT_IST_INDEX: u8 = 1;

const STACK_SIZE: usize = 32 * 1024;

#[repr(C, align(16))]
struct KernelStack([u8; STACK_SIZE]);

static mut PRIVILEGE_STACK: KernelStack = KernelStack([0; STACK_SIZE]);
static mut DOUBLE_FAULT_STACK: KernelStack = KernelStack([0; STACK_SIZE]);

#[repr(C, packed)]
pub struct TaskStateSegment {
    pub reserved1: u32,
    pub rsp: [u64; 3],
    pub reserved2: u64,
    pub ist: [u64; 7],
    pub reserved3: u64,
    pub reserved4: u16,
    pub iomap_base: u16,
}

#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

static mut TSS: TaskStateSegment = TaskStateSegment {
    reserved1: 0,
    rsp: [0; 3],
    reserved2: 0,
    ist: [0; 7],
    reserved3: 0,
    reserved4: 0,
    iomap_base: 104,
};

/// 7-entry GDT:
/// [0] 0x00 Null
/// [1] 0x08 Kernel Code (64-bit, DPL 0)
/// [2] 0x10 Kernel Data (DPL 0)
/// [3] 0x18 User Data   (DPL 3)
/// [4] 0x20 User Code   (64-bit, DPL 3)
/// [5..6] 0x28 64-bit TSS (16-byte system descriptor)
static mut GDT: [u64; 7] = [
    0x0000_0000_0000_0000, // 0x00: Null
    0x00AF_9A00_0000_FFFF, // 0x08: Kernel Code 64
    0x00CF_9200_0000_FFFF, // 0x10: Kernel Data
    0x00CF_F200_0000_FFFF, // 0x18: User Data
    0x00AF_FA00_0000_FFFF, // 0x20: User Code 64
    0x0000_0000_0000_0000, // 0x28: TSS Low (filled at runtime)
    0x0000_0000_0000_0000, // 0x30: TSS High (filled at runtime)
];

/// Initialize the 64-bit TSS stacks, populate the TSS descriptor in the GDT,
/// load GDTR, reload CS/DS/ES/FS/GS/SS, and load TR.
pub fn init() {
    unsafe {
        let priv_stack_top =
            (addr_of!(PRIVILEGE_STACK.0) as u64 + STACK_SIZE as u64) & !0xFu64;
        let df_stack_top =
            (addr_of!(DOUBLE_FAULT_STACK.0) as u64 + STACK_SIZE as u64) & !0xFu64;

        let tss_ptr = addr_of_mut!(TSS);
        (*tss_ptr).rsp[0] = priv_stack_top;
        (*tss_ptr).ist[(DOUBLE_FAULT_IST_INDEX - 1) as usize] = df_stack_top;
        (*tss_ptr).iomap_base = core::mem::size_of::<TaskStateSegment>() as u16;

        let tss_base = tss_ptr as u64;
        let tss_limit = (core::mem::size_of::<TaskStateSegment>() - 1) as u64;

        let tss_low: u64 = (tss_limit & 0xFFFF)
            | ((tss_base & 0xFF_FFFF) << 16)
            | (0x89u64 << 40)
            | (((tss_limit >> 16) & 0xF) << 48)
            | (((tss_base >> 24) & 0xFF) << 56);
        let tss_high: u64 = (tss_base >> 32) & 0xFFFF_FFFF;

        let gdt_ptr = addr_of_mut!(GDT) as *mut u64;
        gdt_ptr.add(5).write(tss_low);
        gdt_ptr.add(6).write(tss_high);

        let gdtr = DescriptorTablePointer {
            limit: (core::mem::size_of::<[u64; 7]>() - 1) as u16,
            base: addr_of!(GDT) as u64,
        };

        asm!(
            "lgdt [{gdtr}]",
            "push {kcode}",
            "lea rax, [rip + 2f]",
            "push rax",
            "retfq",
            "2:",
            "mov ds, {kdata:x}",
            "mov es, {kdata:x}",
            "mov fs, {zero:x}",
            "mov gs, {zero:x}",
            "mov ss, {kdata:x}",
            "ltr {tss:x}",
            gdtr = in(reg) &gdtr,
            kcode = const KERNEL_CODE_SELECTOR as usize,
            kdata = in(reg) KERNEL_DATA_SELECTOR,
            zero = in(reg) 0u16,
            tss = in(reg) TSS_SELECTOR,
            out("rax") _,
            options(preserves_flags),
        );
    }
}
