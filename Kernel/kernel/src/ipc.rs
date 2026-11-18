//! Celebron OS Fast Inter-Process Communication (IPC) & Shared Memory Ring Buffers.
//! Provides two core microkernel primitives:
//! 1. Fast Synchronous Rendezvous IPC: 48-byte register-direct message passing with
//!    unforgeable sender badging.
//! 2. Zero-Copy Shared-Memory Ring Buffer: Lockless Single-Producer Single-Consumer (SPSC)
//!    buffer for high-bandwidth streaming (storage blocks, network packets, display frames).

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub const MAX_IPC_CHANNELS: usize = 16;
pub const RING_BUFFER_CAPACITY: usize = 64;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcError {
    ChannelNotFound,
    ChannelFull,
    NoMessageAvailable,
    PermissionDenied,
    InvalidBuffer,
}

/// 48-byte fast microkernel register message.
/// On `syscall`, arguments are passed in registers:
/// - `sender_badge`: injected by the kernel from the sender's capability badge (cannot be forged)
/// - `label`: protocol message ID (e.g. 0x01 = ping, 0x02 = echo, 0x1337 = cognitive intent)
/// - `data`: 4 x 64-bit general-purpose data words
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpcMessage {
    pub sender_badge: u64,
    pub label: u64,
    pub data: [u64; 4],
}

impl IpcMessage {
    pub const fn empty() -> Self {
        Self {
            sender_badge: 0,
            label: 0,
            data: [0; 4],
        }
    }

    pub const fn new(sender_badge: u64, label: u64, d0: u64, d1: u64, d2: u64, d3: u64) -> Self {
        Self {
            sender_badge,
            label,
            data: [d0, d1, d2, d3],
        }
    }
}

/// Synchronous rendezvous IPC endpoint channel.
struct IpcEndpoint {
    active: bool,
    channel_id: u64,
    has_pending: bool,
    pending: IpcMessage,
}

impl IpcEndpoint {
    const fn empty() -> Self {
        Self {
            active: false,
            channel_id: 0,
            has_pending: false,
            pending: IpcMessage::empty(),
        }
    }
}

static mut CHANNELS: [IpcEndpoint; MAX_IPC_CHANNELS] = [const { IpcEndpoint::empty() }; MAX_IPC_CHANNELS];

static IPC_TOTAL_MESSAGES: AtomicU64 = AtomicU64::new(0);
static IPC_RENDEZVOUS_COUNT: AtomicU64 = AtomicU64::new(0);
static IPC_RING_BUFFER_OPS: AtomicU64 = AtomicU64::new(0);

pub fn init_channels() {
    unsafe {
        let channels = &mut *addr_of_mut!(CHANNELS);
        // Channel 1: Init task rendezvous endpoint
        channels[0].active = true;
        channels[0].channel_id = 1;
        channels[0].has_pending = false;

        // Channel 2: Cognitive Intent Bus endpoint
        channels[1].active = true;
        channels[1].channel_id = 2;
        channels[1].has_pending = false;
    }
}

/// Deliver a message into the specified IPC channel.
pub fn channel_send(channel_id: u64, msg: IpcMessage) -> Result<(), IpcError> {
    unsafe {
        let channels = &mut *addr_of_mut!(CHANNELS);
        for ch in channels.iter_mut() {
            if ch.active && ch.channel_id == channel_id {
                if ch.has_pending {
                    return Err(IpcError::ChannelFull);
                }
                ch.pending = msg;
                ch.has_pending = true;
                IPC_TOTAL_MESSAGES.fetch_add(1, Ordering::Relaxed);
                IPC_RENDEZVOUS_COUNT.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
        }
    }
    Err(IpcError::ChannelNotFound)
}

/// Receive a message from the specified IPC channel.
pub fn channel_recv(channel_id: u64) -> Result<IpcMessage, IpcError> {
    unsafe {
        let channels = &mut *addr_of_mut!(CHANNELS);
        for ch in channels.iter_mut() {
            if ch.active && ch.channel_id == channel_id {
                if !ch.has_pending {
                    return Err(IpcError::NoMessageAvailable);
                }
                let msg = ch.pending;
                ch.has_pending = false;
                ch.pending = IpcMessage::empty();
                IPC_TOTAL_MESSAGES.fetch_add(1, Ordering::Relaxed);
                return Ok(msg);
            }
        }
    }
    Err(IpcError::ChannelNotFound)
}

/// Zero-Copy Lockless Single-Producer Single-Consumer (SPSC) Shared Memory Ring Buffer.
/// Used for high-bandwidth user-to-user and driver-to-driver bulk communication.
#[repr(C, align(64))]
pub struct SharedRingBuffer {
    pub magic: u32,       // 0x52494E47 ("RING")
    pub capacity: u32,
    pub head: AtomicU32,  // Producer cursor
    pub tail: AtomicU32,  // Consumer cursor
    pub dropped: AtomicU32,
    _pad: [u8; 44],       // Align buffer to 64-byte cacheline boundary
    pub storage: [u64; RING_BUFFER_CAPACITY],
}

impl SharedRingBuffer {
    pub const MAGIC: u32 = 0x52494E47;

    pub const fn new() -> Self {
        Self {
            magic: Self::MAGIC,
            capacity: RING_BUFFER_CAPACITY as u32,
            head: AtomicU32::new(0),
            tail: AtomicU32::new(0),
            dropped: AtomicU32::new(0),
            _pad: [0; 44],
            storage: [0; RING_BUFFER_CAPACITY],
        }
    }

    /// Enqueue a 64-bit word (called by producer).
    pub fn push(&mut self, item: u64) -> bool {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);

        let next_head = (head + 1) % (RING_BUFFER_CAPACITY as u32);
        if next_head == tail {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }

        self.storage[head as usize] = item;
        self.head.store(next_head, Ordering::Release);
        IPC_RING_BUFFER_OPS.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Dequeue a 64-bit word (called by consumer).
    pub fn pop(&mut self) -> Option<u64> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);

        if tail == head {
            return None;
        }

        let item = self.storage[tail as usize];
        let next_tail = (tail + 1) % (RING_BUFFER_CAPACITY as u32);
        self.tail.store(next_tail, Ordering::Release);
        IPC_RING_BUFFER_OPS.fetch_add(1, Ordering::Relaxed);
        Some(item)
    }

    pub fn is_empty(&self) -> bool {
        self.tail.load(Ordering::Relaxed) == self.head.load(Ordering::Relaxed)
    }
}

#[allow(dead_code)]
pub struct IpcStats {
    pub total_messages: u64,
    pub rendezvous_count: u64,
    pub ring_buffer_ops: u64,
}

#[allow(dead_code)]
pub fn stats() -> IpcStats {
    IpcStats {
        total_messages: IPC_TOTAL_MESSAGES.load(Ordering::Relaxed),
        rendezvous_count: IPC_RENDEZVOUS_COUNT.load(Ordering::Relaxed),
        ring_buffer_ops: IPC_RING_BUFFER_OPS.load(Ordering::Relaxed),
    }
}

/// Run an in-kernel self-test verifying Rendezvous IPC and SPSC Ring Buffer semantics.
pub fn run_self_test() -> bool {
    init_channels();

    // 1. Send message on Channel 1
    let send_msg = IpcMessage::new(0xCAFE_0001, 0x1337, 10, 20, 30, 40);
    assert!(channel_send(1, send_msg).is_ok());

    // 2. Channel should be full for second send
    assert_eq!(channel_send(1, send_msg), Err(IpcError::ChannelFull));

    // 3. Receive message and verify exact fields & badge
    let recv_msg = channel_recv(1).expect("channel_recv failed");
    assert_eq!(recv_msg.sender_badge, 0xCAFE_0001);
    assert_eq!(recv_msg.label, 0x1337);
    assert_eq!(recv_msg.data, [10, 20, 30, 40]);

    // 4. Test Lockless SPSC Ring Buffer
    let mut ring = SharedRingBuffer::new();
    assert!(ring.is_empty());
    for i in 0u64..32 {
        assert!(ring.push(i * 100));
    }
    for i in 0u64..32 {
        assert_eq!(ring.pop(), Some(i * 100));
    }
    assert!(ring.is_empty());

    true
}
