//! Celebron OS 64-bit Interrupt Descriptor Table (IDT) & Assembly ISR Dispatch.
//! Uses stable `global_asm!` entry stubs with guaranteed 16-byte stack alignment.

use crate::gdt::{DOUBLE_FAULT_IST_INDEX, KERNEL_CODE_SELECTOR};
use crate::kprintln;
use core::arch::{asm, global_asm};
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicU64, Ordering};

const IDT_ENTRIES: usize = 256;
const IDT_INTERRUPT_GATE_RING0: u8 = 0x8E;

static BREAKPOINT_COUNT: AtomicU64 = AtomicU64::new(0);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IdtEntry {
    pub offset_low: u16,
    pub selector: u16,
    pub ist: u8,
    pub type_attr: u8,
    pub offset_mid: u16,
    pub offset_high: u32,
    pub reserved: u32,
}

impl IdtEntry {
    pub const fn empty() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            ist: 0,
            type_attr: 0,
            offset_mid: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    pub fn set_handler(&mut self, handler_addr: u64, selector: u16, ist: u8, type_attr: u8) {
        self.offset_low = (handler_addr & 0xFFFF) as u16;
        self.selector = selector;
        self.ist = ist & 0x7;
        self.type_attr = type_attr;
        self.offset_mid = ((handler_addr >> 16) & 0xFFFF) as u16;
        self.offset_high = ((handler_addr >> 32) & 0xFFFF_FFFF) as u32;
        self.reserved = 0;
    }
}

#[repr(C, packed)]
struct IdtPointer {
    limit: u16,
    base: u64,
}

#[repr(C)]
pub struct InterruptFrame {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub vector: u64,
    pub error_code: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

static mut IDT: [IdtEntry; IDT_ENTRIES] = [IdtEntry::empty(); IDT_ENTRIES];

global_asm!(
    r#"
.macro ISR_NOERR vec
.global isr_stub_\vec
isr_stub_\vec:
    push 0
    push \vec
    jmp isr_common_stub
.endm

.macro ISR_ERR vec
.global isr_stub_\vec
isr_stub_\vec:
    push \vec
    jmp isr_common_stub
.endm

ISR_NOERR 0
ISR_NOERR 1
ISR_NOERR 2
ISR_NOERR 3
ISR_NOERR 4
ISR_NOERR 5
ISR_NOERR 6
ISR_NOERR 7
ISR_ERR   8
ISR_NOERR 9
ISR_ERR   10
ISR_ERR   11
ISR_ERR   12
ISR_ERR   13
ISR_ERR   14
ISR_NOERR 15
ISR_NOERR 16
ISR_ERR   17
ISR_NOERR 18
ISR_NOERR 19
ISR_NOERR 20
ISR_ERR   21
ISR_NOERR 22
ISR_NOERR 23
ISR_NOERR 24
ISR_NOERR 25
ISR_NOERR 26
ISR_NOERR 27
ISR_NOERR 28
ISR_ERR   29
ISR_ERR   30
ISR_NOERR 31
ISR_NOERR 32
ISR_NOERR 255

.global isr_stub_unhandled
isr_stub_unhandled:
    push 0
    push 254
    jmp isr_common_stub

isr_common_stub:
    cld
    push r15
    push r14
    push r13
    push r12
    push r11
    push r10
    push r9
    push r8
    push rbp
    push rdi
    push rsi
    push rdx
    push rcx
    push rbx
    push rax

    mov rdi, rsp
    call rust_interrupt_dispatch
    mov rsp, rax

    pop rax
    pop rbx
    pop rcx
    pop rdx
    pop rsi
    pop rdi
    pop rbp
    pop r8
    pop r9
    pop r10
    pop r11
    pop r12
    pop r13
    pop r14
    pop r15

    add rsp, 16
    iretq
"#
);

extern "C" {
    fn isr_stub_0();
    fn isr_stub_1();
    fn isr_stub_2();
    fn isr_stub_3();
    fn isr_stub_4();
    fn isr_stub_5();
    fn isr_stub_6();
    fn isr_stub_7();
    fn isr_stub_8();
    fn isr_stub_9();
    fn isr_stub_10();
    fn isr_stub_11();
    fn isr_stub_12();
    fn isr_stub_13();
    fn isr_stub_14();
    fn isr_stub_15();
    fn isr_stub_16();
    fn isr_stub_17();
    fn isr_stub_18();
    fn isr_stub_19();
    fn isr_stub_20();
    fn isr_stub_21();
    fn isr_stub_22();
    fn isr_stub_23();
    fn isr_stub_24();
    fn isr_stub_25();
    fn isr_stub_26();
    fn isr_stub_27();
    fn isr_stub_28();
    fn isr_stub_29();
    fn isr_stub_30();
    fn isr_stub_31();
    fn isr_stub_32();
    fn isr_stub_255();
    fn isr_stub_unhandled();
}

fn exception_name(vector: u64) -> &'static str {
    match vector {
        0 => "#DE Divide-by-Zero",
        1 => "#DB Debug",
        2 => "NMI Interrupt",
        3 => "#BP Breakpoint",
        4 => "#OF Overflow",
        5 => "#BR Bound Range Exceeded",
        6 => "#UD Invalid Opcode",
        7 => "#NM Device Not Available",
        8 => "#DF Double Fault",
        9 => "Coprocessor Segment Overrun",
        10 => "#TS Invalid TSS",
        11 => "#NP Segment Not Present",
        12 => "#SS Stack-Segment Fault",
        13 => "#GP General Protection Fault",
        14 => "#PF Page Fault",
        16 => "#MF x87 Floating-Point Exception",
        17 => "#AC Alignment Check",
        18 => "#MC Machine Check",
        19 => "#XM SIMD Floating-Point Exception",
        20 => "#VE Virtualization Exception",
        21 => "#CP Control Protection Exception",
        29 => "#VC VMM Communication Exception",
        30 => "#SX Security Exception",
        _ => "Reserved CPU Exception",
    }
}

/// Central Ring-0 interrupt & CPU exception dispatcher invoked by `isr_common_stub`.
/// Returns the `InterruptFrame` stack pointer (`RSP`) of the thread to resume.
#[no_mangle]
pub extern "sysv64" fn rust_interrupt_dispatch(
    frame_ptr: *mut InterruptFrame,
) -> *mut InterruptFrame {
    let frame = unsafe { &mut *frame_ptr };
    match frame.vector {
        3 => {
            BREAKPOINT_COUNT.fetch_add(1, Ordering::Relaxed);
            kprintln!(
                "[CELEBRON-IDT] Caught #BP Breakpoint Exception (RIP=0x{:016X}, CS=0x{:04X})",
                frame.rip,
                frame.cs
            );
            frame_ptr
        }
        32 => {
            crate::interrupts::on_timer_tick();
            crate::scheduler::schedule_on_tick(frame_ptr)
        }
        255 => {
            // APIC Spurious Interrupt: per Intel SDM, do NOT send EOI.
            frame_ptr
        }
        254 => {
            kprintln!(
                "[CELEBRON-IDT] Unhandled interrupt vector received (RIP=0x{:016X})",
                frame.rip
            );
            frame_ptr
        }
        vec if vec < 32 => {
            let cr2: u64;
            unsafe {
                asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack, preserves_flags));
            }
            kprintln!("\n================ [FATAL RING-0 CPU EXCEPTION] ================");
            kprintln!(
                "Vector {}: {} | ErrorCode=0x{:016X}",
                vec,
                exception_name(vec),
                frame.error_code
            );
            kprintln!(
                "RIP=0x{:016X}  CS=0x{:04X}  RFLAGS=0x{:016X}",
                frame.rip,
                frame.cs,
                frame.rflags
            );
            kprintln!(
                "RSP=0x{:016X}  SS=0x{:04X}  CR2=0x{:016X}",
                frame.rsp,
                frame.ss,
                cr2
            );
            kprintln!(
                "RAX=0x{:016X}  RBX=0x{:016X}  RCX=0x{:016X}  RDX=0x{:016X}",
                frame.rax,
                frame.rbx,
                frame.rcx,
                frame.rdx
            );
            kprintln!(
                "RSI=0x{:016X}  RDI=0x{:016X}  RBP=0x{:016X}",
                frame.rsi,
                frame.rdi,
                frame.rbp
            );
            kprintln!("==============================================================");
            loop {
                unsafe {
                    asm!("cli; hlt", options(nomem, nostack, preserves_flags));
                }
            }
        }
        other => {
            kprintln!("[CELEBRON-IDT] Unexpected vector {}", other);
            frame_ptr
        }
    }
}

/// Populate all 256 IDT gates and load `IDTR`.
pub fn init() {
    let exception_stubs: [unsafe extern "C" fn(); 32] = [
        isr_stub_0, isr_stub_1, isr_stub_2, isr_stub_3,
        isr_stub_4, isr_stub_5, isr_stub_6, isr_stub_7,
        isr_stub_8, isr_stub_9, isr_stub_10, isr_stub_11,
        isr_stub_12, isr_stub_13, isr_stub_14, isr_stub_15,
        isr_stub_16, isr_stub_17, isr_stub_18, isr_stub_19,
        isr_stub_20, isr_stub_21, isr_stub_22, isr_stub_23,
        isr_stub_24, isr_stub_25, isr_stub_26, isr_stub_27,
        isr_stub_28, isr_stub_29, isr_stub_30, isr_stub_31,
    ];

    unsafe {
        let idt_ptr = addr_of_mut!(IDT) as *mut IdtEntry;

        for vec in 0..32usize {
            let ist = if vec == 8 { DOUBLE_FAULT_IST_INDEX } else { 0 };
            (&mut *idt_ptr.add(vec)).set_handler(
                exception_stubs[vec] as usize as u64,
                KERNEL_CODE_SELECTOR,
                ist,
                IDT_INTERRUPT_GATE_RING0,
            );
        }

        // Vector 32 (0x20): Local APIC Timer
        (&mut *idt_ptr.add(32)).set_handler(
            isr_stub_32 as *const () as usize as u64,
            KERNEL_CODE_SELECTOR,
            0,
            IDT_INTERRUPT_GATE_RING0,
        );

        // Vectors 33..254: Safe unhandled fallback
        for vec in 33..255usize {
            (&mut *idt_ptr.add(vec)).set_handler(
                isr_stub_unhandled as *const () as usize as u64,
                KERNEL_CODE_SELECTOR,
                0,
                IDT_INTERRUPT_GATE_RING0,
            );
        }

        // Vector 255 (0xFF): Local APIC Spurious Interrupt
        (&mut *idt_ptr.add(255)).set_handler(
            isr_stub_255 as *const () as usize as u64,
            KERNEL_CODE_SELECTOR,
            0,
            IDT_INTERRUPT_GATE_RING0,
        );

        let idtr = IdtPointer {
            limit: (core::mem::size_of::<[IdtEntry; IDT_ENTRIES]>() - 1) as u16,
            base: addr_of!(IDT) as u64,
        };

        asm!("lidt [{idtr}]", idtr = in(reg) &idtr, options(nostack, preserves_flags));
    }
}

/// Trigger a controlled `#BP` (`int3`) software trap and verify clean IDT dispatch & recovery.
pub fn run_breakpoint_self_test() -> u64 {
    let before = BREAKPOINT_COUNT.load(Ordering::Relaxed);
    unsafe {
        asm!("int3", options(nomem, nostack));
    }
    let after = BREAKPOINT_COUNT.load(Ordering::Relaxed);
    assert!(
        after == before + 1,
        "#BP int3 IDT self-test failed: counter did not increment"
    );
    after
}
