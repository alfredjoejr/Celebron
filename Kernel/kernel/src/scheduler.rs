//! Celebron OS Preemptive Priority-Aware Kernel Thread Scheduler.
//! Manages Ring-0 kernel threads, allocates isolated per-thread kernel stacks on the
//! higher-half heap (`0xFFFF_C000_0000_0000`), and performs hardware-driven preemptive
//! context switches inside the Local APIC Timer ISR (Vector 32).

use crate::gdt::{KERNEL_CODE_SELECTOR, KERNEL_DATA_SELECTOR};
use crate::idt::InterruptFrame;
use alloc::vec;
use alloc::vec::Vec;
use core::arch::asm;
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub const MAX_THREADS: usize = 16;
const THREAD_STACK_SIZE: usize = 16 * 1024; // 16 KiB per kernel worker thread

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ThreadPriority {
    Low = 1,
    Normal = 2,
    High = 4,
}

impl ThreadPriority {
    pub const fn time_quantum_ticks(self) -> u64 {
        match self {
            ThreadPriority::Low => 1,
            ThreadPriority::Normal => 1,
            ThreadPriority::High => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadState {
    Unused,
    Ready,
    Running,
}

pub struct ThreadControlBlock {
    pub tid: usize,
    pub name: &'static str,
    pub priority: ThreadPriority,
    pub state: ThreadState,
    pub saved_rsp: u64,
    pub ticks_executed: u64,
    pub remaining_quantum: u64,
    /// Owned heap-allocated stack for spawned threads (`None` for TID 0 boot thread).
    pub _stack: Option<Vec<u8>>,
}

impl ThreadControlBlock {
    pub const fn empty() -> Self {
        Self {
            tid: 0,
            name: "unused",
            priority: ThreadPriority::Normal,
            state: ThreadState::Unused,
            saved_rsp: 0,
            ticks_executed: 0,
            remaining_quantum: 1,
            _stack: None,
        }
    }
}

struct SchedulerState {
    threads: [ThreadControlBlock; MAX_THREADS],
    current_idx: usize,
    thread_count: usize,
}

static mut SCHEDULER: SchedulerState = SchedulerState {
    threads: [const { ThreadControlBlock::empty() }; MAX_THREADS],
    current_idx: 0,
    thread_count: 0,
};

static PREEMPTION_ENABLED: AtomicBool = AtomicBool::new(false);
static CONTEXT_SWITCH_COUNT: AtomicU64 = AtomicU64::new(0);
static WORKER_TELEMETRY_OPS: AtomicU64 = AtomicU64::new(0);
static WORKER_COGNITIVE_OPS: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
pub struct SchedulerStats {
    pub active_threads: usize,
    pub context_switches: u64,
    pub worker_telemetry_ops: u64,
    pub worker_cognitive_ops: u64,
}

/// Entry function for `TID 1` (`kworker-telemetry`, High Priority).
/// Executes one unit of background telemetry verification per scheduled slice and
/// sleeps via `hlt` until the next Local APIC Timer interrupt preempts it.
extern "sysv64" fn worker_telemetry_thread(seed: u64) -> ! {
    let mut acc = seed;
    loop {
        acc = acc.wrapping_mul(6364136223846793005).wrapping_add(1);
        core::hint::black_box(acc);
        WORKER_TELEMETRY_OPS.fetch_add(1, Ordering::Relaxed);
        unsafe {
            asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Entry function for `TID 2` (`kworker-cognitive`, Normal Priority).
/// Simulates the background Cognitive Core / Semantic FS indexer heartbeat.
extern "sysv64" fn worker_cognitive_thread(seed: u64) -> ! {
    let mut acc = seed;
    loop {
        acc = acc.rotate_left(13) ^ 0xCE1E_B201_2026_0004;
        core::hint::black_box(acc);
        WORKER_COGNITIVE_OPS.fetch_add(1, Ordering::Relaxed);
        unsafe {
            asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Spawn a new Ring-0 kernel thread with a 16 KiB heap-allocated stack and synthetic `InterruptFrame`.
pub fn spawn_kernel_thread(
    name: &'static str,
    priority: ThreadPriority,
    entry: extern "sysv64" fn(u64) -> !,
    arg: u64,
) -> usize {
    unsafe {
        let sched = &mut *addr_of_mut!(SCHEDULER);
        let idx = sched.thread_count;
        assert!(idx < MAX_THREADS, "Maximum kernel threads reached");

        let mut stack = vec![0u8; THREAD_STACK_SIZE];
        let stack_base = stack.as_mut_ptr() as u64;
        let stack_top = (stack_base + THREAD_STACK_SIZE as u64) & !0xFu64;

        // System V AMD64 ABI requires `RSP % 16 == 8` at function entry (simulating the 8-byte
        // return address pushed by `call`).
        let thread_initial_rsp = stack_top - 8;

        // Place the initial `InterruptFrame` below `thread_initial_rsp` on a 16-byte boundary.
        let frame_addr =
            (thread_initial_rsp - core::mem::size_of::<InterruptFrame>() as u64) & !0xFu64;
        let frame_ptr = frame_addr as *mut InterruptFrame;

        core::ptr::write(
            frame_ptr,
            InterruptFrame {
                rax: 0,
                rbx: 0,
                rcx: 0,
                rdx: 0,
                rsi: 0,
                rdi: arg, // First argument (`sysv64` ABI)
                rbp: 0,
                r8: 0,
                r9: 0,
                r10: 0,
                r11: 0,
                r12: 0,
                r13: 0,
                r14: 0,
                r15: 0,
                vector: 32,
                error_code: 0,
                rip: entry as *const () as usize as u64,
                cs: KERNEL_CODE_SELECTOR as u64,
                rflags: 0x202, // IF=1 (Interrupts enabled) + bit 1 reserved=1
                rsp: thread_initial_rsp,
                ss: KERNEL_DATA_SELECTOR as u64,
            },
        );

        let tid = idx;
        sched.threads[idx] = ThreadControlBlock {
            tid,
            name,
            priority,
            state: ThreadState::Ready,
            saved_rsp: frame_addr,
            ticks_executed: 0,
            remaining_quantum: priority.time_quantum_ticks(),
            _stack: Some(stack),
        };
        sched.thread_count += 1;
        crate::kprintln!(
            "[CELEBRON-SCHED] Spawned kernel thread TID={} '{}' (priority={:?}, stack_top=0x{:016X})",
            sched.threads[idx].tid,
            sched.threads[idx].name,
            sched.threads[idx].priority,
            stack_top
        );
        tid
    }
}

/// Initialize the scheduler with `TID 0` (`kmain-supervisor`) and spawn background worker threads.
pub fn init_and_spawn_workers() {
    unsafe {
        let sched = &mut *addr_of_mut!(SCHEDULER);
        sched.threads[0] = ThreadControlBlock {
            tid: 0,
            name: "kmain-supervisor",
            priority: ThreadPriority::Normal,
            state: ThreadState::Running,
            saved_rsp: 0,
            ticks_executed: 0,
            remaining_quantum: ThreadPriority::Normal.time_quantum_ticks(),
            _stack: None,
        };
        sched.current_idx = 0;
        sched.thread_count = 1;
    }

    // Spawn TID 1 (High Priority Telemetry Worker) & TID 2 (Normal Priority Cognitive Worker)
    spawn_kernel_thread(
        "kworker-telemetry",
        ThreadPriority::High,
        worker_telemetry_thread,
        0x1001,
    );
    spawn_kernel_thread(
        "kworker-cognitive",
        ThreadPriority::Normal,
        worker_cognitive_thread,
        0x2002,
    );

    PREEMPTION_ENABLED.store(true, Ordering::Release);
}

/// Called from `rust_interrupt_dispatch` on every Local APIC Timer interrupt (Vector 32).
/// Saves `current_rsp` and returns the `InterruptFrame` stack pointer of the next thread to run.
pub fn schedule_on_tick(current_rsp: *mut InterruptFrame) -> *mut InterruptFrame {
    if !PREEMPTION_ENABLED.load(Ordering::Acquire) {
        return current_rsp;
    }

    unsafe {
        let sched = &mut *addr_of_mut!(SCHEDULER);
        let count = sched.thread_count;
        if count <= 1 {
            return current_rsp;
        }

        let cur = sched.current_idx;
        sched.threads[cur].saved_rsp = current_rsp as u64;
        sched.threads[cur].ticks_executed = sched.threads[cur].ticks_executed.saturating_add(1);

        // Decrement current thread's remaining priority quantum
        if sched.threads[cur].remaining_quantum > 1 {
            sched.threads[cur].remaining_quantum -= 1;
            return current_rsp;
        }

        // Quantum expired: mark current thread Ready and pick next Ready thread (Round-Robin)
        sched.threads[cur].state = ThreadState::Ready;
        sched.threads[cur].remaining_quantum = sched.threads[cur].priority.time_quantum_ticks();

        let mut next_idx = cur;
        for step in 1..=count {
            let candidate = (cur + step) % count;
            if sched.threads[candidate].state == ThreadState::Ready {
                next_idx = candidate;
                break;
            }
        }

        sched.threads[next_idx].state = ThreadState::Running;
        sched.current_idx = next_idx;

        if next_idx != cur {
            CONTEXT_SWITCH_COUNT.fetch_add(1, Ordering::Relaxed);
        }

        sched.threads[next_idx].saved_rsp as *mut InterruptFrame
    }
}

pub fn stats() -> SchedulerStats {
    let active_threads = unsafe { (*addr_of_mut!(SCHEDULER)).thread_count };
    SchedulerStats {
        active_threads,
        context_switches: CONTEXT_SWITCH_COUNT.load(Ordering::Relaxed),
        worker_telemetry_ops: WORKER_TELEMETRY_OPS.load(Ordering::Relaxed),
        worker_cognitive_ops: WORKER_COGNITIVE_OPS.load(Ordering::Relaxed),
    }
}
