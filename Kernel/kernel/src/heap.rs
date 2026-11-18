//! Celebron OS Ring-0 Kernel Heap Allocator (`GlobalAlloc`).
//! Implements a spinlock-protected Free-List + Bump allocator backed by
//! VMM-mapped pages in the higher half (`0xFFFF_C000_0000_0000`).

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, Ordering};

const MIN_BLOCK_SIZE: usize = 16;

#[repr(C)]
struct FreeBlock {
    size: usize,
    next: *mut FreeBlock,
}

#[derive(Debug, Clone, Copy)]
pub struct HeapStats {
    pub total_bytes: usize,
    pub used_bytes: usize,
    pub free_bytes: usize,
    pub active_allocations: usize,
}

struct HeapInner {
    heap_start: usize,
    heap_end: usize,
    bump_cursor: usize,
    free_head: *mut FreeBlock,
    free_list_bytes: usize,
    active_allocations: usize,
}

impl HeapInner {
    const fn empty() -> Self {
        Self {
            heap_start: 0,
            heap_end: 0,
            bump_cursor: 0,
            free_head: null_mut(),
            free_list_bytes: 0,
            active_allocations: 0,
        }
    }

    fn init(&mut self, virt_start: usize, size_bytes: usize) {
        self.heap_start = virt_start;
        self.heap_end = virt_start.saturating_add(size_bytes);
        self.bump_cursor = virt_start;
        self.free_head = null_mut();
        self.free_list_bytes = 0;
        self.active_allocations = 0;
    }

    /// Insert a free region `[addr .. addr + size)` into the address-sorted free list,
    /// coalescing adjacent blocks and rewinding `bump_cursor` when possible.
    unsafe fn insert_free_block(&mut self, addr: usize, size: usize) {
        if size < MIN_BLOCK_SIZE {
            return;
        }

        // Find sorted insertion spot (`prev` < `addr` < `curr`)
        let mut prev: *mut FreeBlock = null_mut();
        let mut curr = self.free_head;
        while !curr.is_null() && (curr as usize) < addr {
            prev = curr;
            curr = (*curr).next;
        }

        let new_node = addr as *mut FreeBlock;
        (*new_node).size = size;
        (*new_node).next = curr;

        if prev.is_null() {
            self.free_head = new_node;
        } else {
            (*prev).next = new_node;
        }

        self.free_list_bytes += size;

        // Coalesce `new_node` with `curr` (next neighbor) if contiguous
        if !curr.is_null() && addr + (*new_node).size == curr as usize {
            (*new_node).size += (*curr).size;
            (*new_node).next = (*curr).next;
        }

        // Coalesce `prev` with `new_node` if contiguous
        let tail_node = if !prev.is_null() && (prev as usize) + (*prev).size == addr {
            (*prev).size += (*new_node).size;
            (*prev).next = (*new_node).next;
            prev
        } else {
            new_node
        };

        // If the tail free block touches `bump_cursor`, reclaim it directly into bump space.
        if (*tail_node).next.is_null() && (tail_node as usize) + (*tail_node).size == self.bump_cursor
        {
            self.bump_cursor = tail_node as usize;
            self.free_list_bytes -= (*tail_node).size;
            if tail_node == self.free_head {
                self.free_head = null_mut();
            } else {
                // Find node preceding `tail_node` (which must be `prev` before coalescing)
                let mut scan = self.free_head;
                while !scan.is_null() && (*scan).next != tail_node {
                    scan = (*scan).next;
                }
                if !scan.is_null() {
                    (*scan).next = null_mut();
                }
            }
        }
    }

    unsafe fn alloc_block(&mut self, layout: Layout) -> *mut u8 {
        if self.heap_start == 0 || self.heap_end <= self.heap_start {
            return null_mut();
        }

        let size = align_up(layout.size().max(MIN_BLOCK_SIZE), MIN_BLOCK_SIZE);
        let align = layout.align().max(MIN_BLOCK_SIZE);

        // 1. Search free list for a block that can satisfy both `size` and `align`
        let mut prev: *mut FreeBlock = null_mut();
        let mut curr = self.free_head;

        while !curr.is_null() {
            let block_start = curr as usize;
            let block_size = (*curr).size;
            let block_end = block_start + block_size;
            let aligned_start = align_up(block_start, align);

            if let Some(alloc_end) = aligned_start.checked_add(size) {
                let prefix_len = aligned_start - block_start;
                if alloc_end <= block_end && (prefix_len == 0 || prefix_len >= MIN_BLOCK_SIZE) {
                    let suffix_len = block_end - alloc_end;
                    let next_node = (*curr).next;

                    // Detach `curr` from free list
                    if prev.is_null() {
                        self.free_head = next_node;
                    } else {
                        (*prev).next = next_node;
                    }
                    self.free_list_bytes -= block_size;

                    // Re-insert prefix and suffix remainders if any
                    if prefix_len >= MIN_BLOCK_SIZE {
                        self.insert_free_block(block_start, prefix_len);
                    }
                    if suffix_len >= MIN_BLOCK_SIZE {
                        self.insert_free_block(alloc_end, suffix_len);
                    }

                    self.active_allocations += 1;
                    return aligned_start as *mut u8;
                }
            }

            prev = curr;
            curr = (*curr).next;
        }

        // 2. Fallback to bump allocation
        let aligned_start = align_up(self.bump_cursor, align);
        let alloc_end = match aligned_start.checked_add(size) {
            Some(end) if end <= self.heap_end => end,
            _ => return null_mut(),
        };

        let gap = aligned_start - self.bump_cursor;
        self.bump_cursor = alloc_end;
        if gap >= MIN_BLOCK_SIZE {
            self.insert_free_block(aligned_start - gap, gap);
        }

        self.active_allocations += 1;
        aligned_start as *mut u8
    }

    unsafe fn dealloc_block(&mut self, ptr: *mut u8, layout: Layout) {
        if ptr.is_null() {
            return;
        }
        let addr = ptr as usize;
        if addr < self.heap_start || addr >= self.heap_end {
            return;
        }

        let size = align_up(layout.size().max(MIN_BLOCK_SIZE), MIN_BLOCK_SIZE);
        self.insert_free_block(addr, size);
        self.active_allocations = self.active_allocations.saturating_sub(1);
    }

    fn stats(&self) -> HeapStats {
        let total_bytes = self.heap_end.saturating_sub(self.heap_start);
        let unbumped_bytes = self.heap_end.saturating_sub(self.bump_cursor);
        let free_bytes = unbumped_bytes.saturating_add(self.free_list_bytes);
        let used_bytes = total_bytes.saturating_sub(free_bytes);

        HeapStats {
            total_bytes,
            used_bytes,
            free_bytes,
            active_allocations: self.active_allocations,
        }
    }
}

#[inline(always)]
const fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

struct SpinLock<T> {
    locked: AtomicBool,
    inner: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for SpinLock<T> {}
unsafe impl<T: Send> Send for SpinLock<T> {}

// HeapInner holds raw pointers used solely inside the kernel address space.
unsafe impl Send for HeapInner {}

impl<T> SpinLock<T> {
    const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            inner: UnsafeCell::new(value),
        }
    }

    fn lock(&self) -> SpinLockGuard<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        SpinLockGuard { lock: self }
    }
}

struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> core::ops::Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.inner.get() }
    }
}

impl<T> core::ops::DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.inner.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

pub struct KernelHeapAllocator {
    inner: SpinLock<HeapInner>,
}

impl KernelHeapAllocator {
    pub const fn new() -> Self {
        Self {
            inner: SpinLock::new(HeapInner::empty()),
        }
    }
}

unsafe impl GlobalAlloc for KernelHeapAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.inner.lock().alloc_block(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.inner.lock().dealloc_block(ptr, layout);
    }
}

#[global_allocator]
static KERNEL_HEAP: KernelHeapAllocator = KernelHeapAllocator::new();

pub fn init_kernel_heap(virt_start: usize, size_bytes: usize) {
    KERNEL_HEAP.inner.lock().init(virt_start, size_bytes);
}

pub fn heap_stats() -> HeapStats {
    KERNEL_HEAP.inner.lock().stats()
}
