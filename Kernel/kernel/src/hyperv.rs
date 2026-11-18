//! Celebron OS Clean-Room Hyper-V VMBus, SynIC & Integration Services Driver.
//! Implements Microsoft Hyper-V TLFS hypercalls, Synthetic Interrupt Controller (SynIC),
//! VMBus channel negotiation & GPADL ring buffers, plus three native synthetic devices:
//! 1. **Shutdown Integration Service (IC)** — enables Hyper-V Manager's "Shut Down" button!
//! 2. **Heartbeat Integration Service (IC)** — reports live "OK" heartbeat to Hyper-V Manager!
//! 3. **Synthetic Keyboard (VMBus)** — enables `[S]` Shutdown / `[R]` Reboot / `[W]` Warm Reboot
//!    keys directly inside Hyper-V Generation 2 Virtual Machine Connection!

use crate::interrupts::HYPERV_VMBUS_VECTOR;
use crate::pmm::{PhysicalMemoryManager, PAGE_SIZE};
use crate::power::{self, PowerAction};
use crate::vmm::{VirtualMemoryManager, PAGE_PRESENT, PAGE_WRITABLE};
use core::arch::asm;
use core::ptr::{addr_of_mut, read_unaligned, read_volatile, write_unaligned, write_volatile};
use core::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};

// Hyper-V Synthetic MSRs (TLFS)
const HV_X64_MSR_GUEST_OS_ID: u32 = 0x4000_0000;
const HV_X64_MSR_HYPERCALL: u32 = 0x4000_0001;
const HV_X64_MSR_SCONTROL: u32 = 0x4000_0080;
const HV_X64_MSR_SIEFP: u32 = 0x4000_0082;
const HV_X64_MSR_SIMP: u32 = 0x4000_0083;
const HV_X64_MSR_EOM: u32 = 0x4000_0084;
const HV_X64_MSR_SINT2: u32 = 0x4000_0092;

// Celebron OS Guest OS ID (Bit 63 = Open/Independent OS, OS Type = 1, Version = 0.1)
const CELEBRON_GUEST_OS_ID: u64 = (1u64 << 63) | (0x01u64 << 48) | (0x0001u64 << 16) | 1;

// Hyper-V Hypercall Codes
const HVCALL_POST_MESSAGE: u64 = 0x005C;
const HVCALL_SIGNAL_EVENT_FAST: u64 = 0x005D | (1u64 << 16);
const HV_STATUS_INSUFFICIENT_BUFFERS: u16 = 0x0013;

// SynIC / VMBus Message Constants
const VMBUS_SINT_INDEX: usize = 2;
const HVMSG_NONE: u32 = 0;
const HVMSG_VMBUS: u32 = 1;
const VMBUS_CONN_ID_LEGACY: u32 = 1;
const VMBUS_CONN_ID_WIN10: u32 = 4;

// VMBus Protocol Versions
const VMBUS_VERSION_WIN10_V5_1: u32 = (5 << 16) | 1;
const VMBUS_VERSION_WIN10: u32 = 4 << 16;
const VMBUS_VERSION_WIN8_1: u32 = 3 << 16;

// VMBus Channel Control Message Types
const CHANNELMSG_OFFERCHANNEL: u32 = 1;
const CHANNELMSG_REQUESTOFFERS: u32 = 3;
const CHANNELMSG_ALLOFFERS_DELIVERED: u32 = 4;
const CHANNELMSG_OPENCHANNEL: u32 = 5;
const CHANNELMSG_OPENCHANNEL_RESULT: u32 = 6;
const CHANNELMSG_GPADL_HEADER: u32 = 8;
const CHANNELMSG_GPADL_CREATED: u32 = 10;
const CHANNELMSG_INITIATE_CONTACT: u32 = 14;
const CHANNELMSG_VERSION_RESPONSE: u32 = 15;

// Offer Flags
const VMBUS_CHANNEL_NAMED_PIPE_MODE: u16 = 0x0010;

// Ring Buffer Layout: 1 control page + 4 data pages = 5 pages per direction (10 pages total)
const RING_DATA_PAGES: usize = 4;
const RING_PAGES_PER_DIR: usize = 1 + RING_DATA_PAGES;
const CHANNEL_TOTAL_PAGES: usize = RING_PAGES_PER_DIR * 2;
const RING_DATA_SIZE: usize = RING_DATA_PAGES * 4096;
const VMBUS_RING_VIRT_BASE: u64 = 0xFFFF_D000_0000_0000;
const VMBUS_RING_VIRT_STRIDE: u64 = 0x10000; // 64 KiB per channel slot

// VMBus Ring Packet Types & Flags
const VM_PKT_DATA_INBAND: u16 = 6;
const VM_PKT_FLAG_COMPLETION_REQ: u16 = 1;

// IC Message Types & Flags
const ICMSGTYPE_NEGOTIATE: u16 = 0;
const ICMSGTYPE_HEARTBEAT: u16 = 1;
const ICMSGTYPE_SHUTDOWN: u16 = 3;
const ICMSGHDRFLAG_TRANSACTION: u8 = 1;
const ICMSGHDRFLAG_RESPONSE: u8 = 4;

// Synthetic Keyboard Protocol Constants
const SYNTH_KBD_PROTOCOL_REQUEST: u32 = 1;
const SYNTH_KBD_PROTOCOL_RESPONSE: u32 = 2;
const SYNTH_KBD_EVENT: u32 = 3;
const SYNTH_KBD_VERSION_WIN8: u32 = 1 << 16;
const KEYSTROKE_IS_BREAK: u32 = 1 << 1;

// Microsoft Mixed-Endian VMBus Interface GUIDs
/// Shutdown IC: `{0e0b6031-5213-4934-818b-38d90ced39db}`
const GUID_SHUTDOWN_IC: [u8; 16] = [
    0x31, 0x60, 0x0b, 0x0e, 0x13, 0x52, 0x34, 0x49, 0x81, 0x8b, 0x38, 0xd9, 0x0c, 0xed, 0x39, 0xdb,
];
/// Heartbeat IC: `{57164f39-9115-4e78-ab55-382f3bd5422d}`
const GUID_HEARTBEAT_IC: [u8; 16] = [
    0x39, 0x4f, 0x16, 0x57, 0x15, 0x91, 0x78, 0x4e, 0xab, 0x55, 0x38, 0x2f, 0x3b, 0xd5, 0x42, 0x2d,
];
/// Synthetic Keyboard: `{f912ad6d-2b17-48ea-bd65-f927a61c7684}`
const GUID_SYNTH_KEYBOARD: [u8; 16] = [
    0x6d, 0xad, 0x12, 0xf9, 0x17, 0x2b, 0xea, 0x48, 0xbd, 0x65, 0xf9, 0x27, 0xa6, 0x1c, 0x76, 0x84,
];

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct HvInputPostMessage {
    connection_id: u32,
    reserved: u32,
    message_type: u32,
    payload_size: u32,
    payload: [u8; 240],
}

const _: () = assert!(core::mem::size_of::<HvInputPostMessage>() == 256);

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct HvMessageHeader {
    message_type: u32,
    payload_size: u8,
    message_flags: u8,
    reserved: u16,
    sender: u64,
}

const _: () = assert!(core::mem::size_of::<HvMessageHeader>() == 16);

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct PacketDescriptor {
    packet_type: u16,
    data_offset8: u16,
    length8: u16,
    flags: u16,
    transaction_id: u64,
}

const _: () = assert!(core::mem::size_of::<PacketDescriptor>() == 16);

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IcVersion {
    major: u16,
    minor: u16,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IcMsgHdr {
    framework_version: IcVersion,
    message_type: u16,
    message_version: IcVersion,
    message_size: u16,
    status: u32,
    transaction_id: u8,
    flags: u8,
    reserved: [u8; 2],
}

const _: () = assert!(core::mem::size_of::<IcMsgHdr>() == 20);

#[derive(Clone, Copy)]
struct VmbusChannel {
    offered: bool,
    open: bool,
    negotiated: bool,
    is_pipe: bool,
    channel_id: u32,
    connection_id: u32,
    gpadl_id: u32,
    send_ring_virt: u64,
    recv_ring_virt: u64,
}

impl VmbusChannel {
    const fn empty() -> Self {
        Self {
            offered: false,
            open: false,
            negotiated: false,
            is_pipe: false,
            channel_id: 0,
            connection_id: 0,
            gpadl_id: 0,
            send_ring_virt: 0,
            recv_ring_virt: 0,
        }
    }
}

struct HyperVState {
    active: bool,
    hypercall_page: u64,
    post_msg_page: u64,
    simp_page: u64,
    siefp_page: u64,
    msg_conn_id: u32,
    negotiated_version: u32,
    total_offers: usize,
    shutdown_ch: VmbusChannel,
    heartbeat_ch: VmbusChannel,
    keyboard_ch: VmbusChannel,
}

static mut HV_STATE: HyperVState = HyperVState {
    active: false,
    hypercall_page: 0,
    post_msg_page: 0,
    simp_page: 0,
    siefp_page: 0,
    msg_conn_id: VMBUS_CONN_ID_LEGACY,
    negotiated_version: 0,
    total_offers: 0,
    shutdown_ch: VmbusChannel::empty(),
    heartbeat_ch: VmbusChannel::empty(),
    keyboard_ch: VmbusChannel::empty(),
};

static SYNIC_IRQ_COUNT: AtomicU64 = AtomicU64::new(0);
static HEARTBEAT_COUNT: AtomicU64 = AtomicU64::new(0);
static KBD_EVENT_COUNT: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
pub struct HyperVStatus {
    pub vmbus_online: bool,
    pub vmbus_version: u32,
    pub offers_discovered: usize,
    pub shutdown_ic_ready: bool,
    pub heartbeat_ic_ready: bool,
    pub synth_kbd_ready: bool,
}

impl HyperVStatus {
    pub const fn offline() -> Self {
        Self {
            vmbus_online: false,
            vmbus_version: 0,
            offers_discovered: 0,
            shutdown_ic_ready: false,
            heartbeat_ic_ready: false,
            synth_kbd_ready: false,
        }
    }
}

#[inline(always)]
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

#[inline(always)]
unsafe fn wrmsr(msr: u32, val: u64) {
    let low = val as u32;
    let high = (val >> 32) as u32;
    asm!(
        "wrmsr",
        in("ecx") msr,
        in("eax") low,
        in("edx") high,
        options(nomem, nostack, preserves_flags)
    );
}

/// Invoke the Hyper-V Hypercall code page (`RCX`=control, `RDX`=input, `R8`=output -> `RAX`=status).
#[inline(never)]
unsafe fn hv_do_hypercall(hypercall_page: u64, control: u64, input: u64, output: u64) -> u64 {
    let status: u64;
    asm!(
        "call rax",
        inout("rax") hypercall_page => status,
        inout("rcx") control => _,
        inout("rdx") input => _,
        inout("r8") output => _,
        out("r9") _,
        out("r10") _,
        out("r11") _,
    );
    status
}

/// Post a VMBus control message to the host via `HvCallPostMessage` (`0x005C`).
unsafe fn hv_post_message(conn_id: u32, payload: &[u8]) -> Result<(), u16> {
    let st = &*addr_of_mut!(HV_STATE);
    if payload.len() > 240 || st.hypercall_page == 0 || st.post_msg_page == 0 {
        return Err(0xFFFF);
    }

    let msg_ptr = st.post_msg_page as *mut HvInputPostMessage;
    core::ptr::write_bytes(msg_ptr as *mut u8, 0, 256);

    let mut input = HvInputPostMessage {
        connection_id: conn_id,
        reserved: 0,
        message_type: HVMSG_VMBUS,
        payload_size: payload.len() as u32,
        payload: [0; 240],
    };
    input.payload[..payload.len()].copy_from_slice(payload);
    write_unaligned(msg_ptr, input);
    fence(Ordering::SeqCst);

    for _retry in 0..64 {
        let res = hv_do_hypercall(st.hypercall_page, HVCALL_POST_MESSAGE, st.post_msg_page, 0);
        let code = (res & 0xFFFF) as u16;
        if code == 0 {
            return Ok(());
        }
        if code != HV_STATUS_INSUFFICIENT_BUFFERS {
            return Err(code);
        }
        for _ in 0..20_000 {
            core::hint::spin_loop();
        }
    }
    Err(HV_STATUS_INSUFFICIENT_BUFFERS)
}

/// Signal a host VMBus channel event via Fast `HvCallSignalEvent` (`0x0001_005D`).
unsafe fn hv_signal_event(connection_id: u32) {
    let st = &*addr_of_mut!(HV_STATE);
    if st.hypercall_page == 0 || connection_id == 0 {
        return;
    }
    fence(Ordering::SeqCst);
    let _ = hv_do_hypercall(
        st.hypercall_page,
        HVCALL_SIGNAL_EVENT_FAST,
        connection_id as u64,
        0,
    );
}

/// Try to pop one pending SynIC message from `SIMP[VMBUS_SINT_INDEX]` without blocking.
unsafe fn try_recv_synic_message(out_buf: &mut [u8; 240]) -> Option<usize> {
    let st = &*addr_of_mut!(HV_STATE);
    if st.simp_page == 0 {
        return None;
    }
    let slot_addr = st.simp_page + (VMBUS_SINT_INDEX as u64) * 256;
    let msg_type = read_volatile(slot_addr as *const u32);
    if msg_type == HVMSG_NONE {
        return None;
    }

    fence(Ordering::Acquire);
    let hdr = read_unaligned(slot_addr as *const HvMessageHeader);
    let payload_len = (hdr.payload_size as usize).min(240);
    let src = (slot_addr + 16) as *const u8;
    core::ptr::copy_nonoverlapping(src, out_buf.as_mut_ptr(), payload_len);

    // Acknowledge slot by zeroing message_type, then write EOM if MessagePending is set
    fence(Ordering::SeqCst);
    write_volatile(slot_addr as *mut u32, HVMSG_NONE);
    fence(Ordering::SeqCst);
    if (hdr.message_flags & 0x01) != 0 {
        wrmsr(HV_X64_MSR_EOM, 0);
    }

    Some(payload_len)
}

/// Wait with a bounded spin timeout for the next SynIC message on SINT2.
unsafe fn wait_synic_message(out_buf: &mut [u8; 240], max_spins: u64) -> Option<usize> {
    for _ in 0..max_spins {
        if let Some(len) = try_recv_synic_message(out_buf) {
            return Some(len);
        }
        core::hint::spin_loop();
    }
    None
}

// ============================================================================
// VMBus Circular Ring Buffer Read / Write Helpers
// ============================================================================

#[inline(always)]
unsafe fn ring_ctrl_u32(ring_virt: u64, word_idx: usize) -> &'static AtomicU32 {
    &*((ring_virt as *const AtomicU32).add(word_idx))
}

unsafe fn ring_copy_from(ring_virt: u64, start_offset: usize, dst: &mut [u8]) {
    let data_base = (ring_virt + 4096) as *const u8;
    let mut off = start_offset % RING_DATA_SIZE;
    let mut rem = dst.len();
    let mut written = 0usize;
    while rem > 0 {
        let chunk = rem.min(RING_DATA_SIZE - off);
        core::ptr::copy_nonoverlapping(data_base.add(off), dst.as_mut_ptr().add(written), chunk);
        off = (off + chunk) % RING_DATA_SIZE;
        written += chunk;
        rem -= chunk;
    }
}

unsafe fn ring_copy_to(ring_virt: u64, start_offset: usize, src: &[u8]) {
    let data_base = (ring_virt + 4096) as *mut u8;
    let mut off = start_offset % RING_DATA_SIZE;
    let mut rem = src.len();
    let mut copied = 0usize;
    while rem > 0 {
        let chunk = rem.min(RING_DATA_SIZE - off);
        core::ptr::copy_nonoverlapping(src.as_ptr().add(copied), data_base.add(off), chunk);
        off = (off + chunk) % RING_DATA_SIZE;
        copied += chunk;
        rem -= chunk;
    }
}

unsafe fn ring_zero(ring_virt: u64, start_offset: usize, len: usize) {
    let data_base = (ring_virt + 4096) as *mut u8;
    let mut off = start_offset % RING_DATA_SIZE;
    let mut rem = len;
    while rem > 0 {
        let chunk = rem.min(RING_DATA_SIZE - off);
        core::ptr::write_bytes(data_base.add(off), 0, chunk);
        off = (off + chunk) % RING_DATA_SIZE;
        rem -= chunk;
    }
}

/// Read one packet from a channel's Host->Guest Receive Ring Buffer.
/// Returns `Some((payload_len, transaction_id))`.
unsafe fn ring_recv_packet(ch: &VmbusChannel, out_payload: &mut [u8]) -> Option<(usize, u64)> {
    if !ch.open || ch.recv_ring_virt == 0 {
        return None;
    }
    let write_idx = ring_ctrl_u32(ch.recv_ring_virt, 0).load(Ordering::Acquire) as usize;
    let read_idx = ring_ctrl_u32(ch.recv_ring_virt, 1).load(Ordering::Relaxed) as usize;
    if read_idx == write_idx {
        return None;
    }

    let mut desc_bytes = [0u8; 16];
    ring_copy_from(ch.recv_ring_virt, read_idx, &mut desc_bytes);
    let desc = read_unaligned(desc_bytes.as_ptr() as *const PacketDescriptor);

    let data_off = (desc.data_offset8 as usize) * 8;
    let total_pkt_len = (desc.length8 as usize) * 8;
    if total_pkt_len < data_off || total_pkt_len + 8 > RING_DATA_SIZE {
        // Corrupt packet: resync read_index to write_idx
        ring_ctrl_u32(ch.recv_ring_virt, 1).store(write_idx as u32, Ordering::Release);
        return None;
    }

    let payload_len = total_pkt_len - data_off;
    let copy_len = payload_len.min(out_payload.len());
    if copy_len > 0 {
        ring_copy_from(
            ch.recv_ring_virt,
            (read_idx + data_off) % RING_DATA_SIZE,
            &mut out_payload[..copy_len],
        );
    }

    // Advance past packet descriptor + payload + 8-byte VMBus footer
    let new_read_idx = (read_idx + total_pkt_len + 8) % RING_DATA_SIZE;
    ring_ctrl_u32(ch.recv_ring_virt, 1).store(new_read_idx as u32, Ordering::Release);

    Some((copy_len, desc.transaction_id))
}

/// Write one in-band packet to a channel's Guest->Host Send Ring Buffer and signal host.
unsafe fn ring_send_packet(ch: &VmbusChannel, payload: &[u8], trans_id: u64, req_completion: bool) -> bool {
    if !ch.open || ch.send_ring_virt == 0 {
        return false;
    }

    let padded_payload_len = (payload.len() + 7) & !7;
    let msg_len = 16 + padded_payload_len;
    let total_bytes = msg_len + 8; // + 8-byte footer

    let old_write_idx = ring_ctrl_u32(ch.send_ring_virt, 0).load(Ordering::Relaxed) as usize;
    let read_idx = ring_ctrl_u32(ch.send_ring_virt, 1).load(Ordering::Acquire) as usize;

    let avail = if old_write_idx >= read_idx {
        RING_DATA_SIZE - (old_write_idx - read_idx)
    } else {
        read_idx - old_write_idx
    };
    if avail <= total_bytes {
        return false;
    }

    let desc = PacketDescriptor {
        packet_type: VM_PKT_DATA_INBAND,
        data_offset8: 2, // 16 / 8
        length8: (msg_len / 8) as u16,
        flags: if req_completion {
            VM_PKT_FLAG_COMPLETION_REQ
        } else {
            0
        },
        transaction_id: trans_id,
    };

    let desc_bytes =
        core::slice::from_raw_parts((&desc as *const PacketDescriptor) as *const u8, 16);
    ring_copy_to(ch.send_ring_virt, old_write_idx, desc_bytes);
    ring_copy_to(ch.send_ring_virt, old_write_idx + 16, payload);

    let pad_bytes = padded_payload_len - payload.len();
    if pad_bytes > 0 {
        ring_zero(ch.send_ring_virt, old_write_idx + 16 + payload.len(), pad_bytes);
    }

    // 8-byte VMBus Ring Footer: { reserved: u32 = 0, offset: u32 = old_write_idx }
    let footer_val: u64 = (old_write_idx as u64) << 32;
    ring_copy_to(
        ch.send_ring_virt,
        old_write_idx + msg_len,
        &footer_val.to_le_bytes(),
    );

    let new_write_idx = (old_write_idx + total_bytes) % RING_DATA_SIZE;
    fence(Ordering::Release);
    ring_ctrl_u32(ch.send_ring_virt, 0).store(new_write_idx as u32, Ordering::Release);
    fence(Ordering::SeqCst);

    // Signal host via Hyper-V Fast HvCallSignalEvent
    hv_signal_event(ch.connection_id);
    true
}

// ============================================================================
// VMBus Channel GPADL Creation & Opening
// ============================================================================

unsafe fn open_vmbus_channel(
    slot_idx: usize,
    ch: &mut VmbusChannel,
    pmm: &mut PhysicalMemoryManager,
    vmm: &mut VirtualMemoryManager,
    label: &str,
) -> bool {
    if !ch.offered {
        return false;
    }

    let msg_conn_id = (*addr_of_mut!(HV_STATE)).msg_conn_id;
    let virt_base = VMBUS_RING_VIRT_BASE + (slot_idx as u64) * VMBUS_RING_VIRT_STRIDE;

    // 1. Allocate 10 zeroed 4 KiB frames and map them contiguously at `virt_base`
    let mut pfns = [0u64; CHANNEL_TOTAL_PAGES];
    for i in 0..CHANNEL_TOTAL_PAGES {
        let phys = match pmm.alloc_frame() {
            Some(p) => p,
            None => return false,
        };
        pfns[i] = phys / PAGE_SIZE;
        vmm.map_4k(
            virt_base + (i as u64) * PAGE_SIZE,
            phys,
            PAGE_PRESENT | PAGE_WRITABLE,
            pmm,
        );
    }

    let gpadl_id = 0x1000u32 + (slot_idx as u32);
    ch.gpadl_id = gpadl_id;
    ch.send_ring_virt = virt_base;
    ch.recv_ring_virt = virt_base + (RING_PAGES_PER_DIR as u64) * PAGE_SIZE;

    // 2. Build & send CHANNELMSG_GPADL_HEADER (108 bytes total)
    let mut gpadl_msg = [0u8; 108];
    gpadl_msg[0..4].copy_from_slice(&CHANNELMSG_GPADL_HEADER.to_le_bytes());
    gpadl_msg[8..12].copy_from_slice(&ch.channel_id.to_le_bytes());
    gpadl_msg[12..16].copy_from_slice(&gpadl_id.to_le_bytes());
    let range_buflen: u16 = (8 + CHANNEL_TOTAL_PAGES * 8) as u16; // 88 bytes
    gpadl_msg[16..18].copy_from_slice(&range_buflen.to_le_bytes());
    gpadl_msg[18..20].copy_from_slice(&1u16.to_le_bytes()); // rangecount = 1
    let total_ring_bytes: u32 = (CHANNEL_TOTAL_PAGES as u32) * 4096; // 40960
    gpadl_msg[20..24].copy_from_slice(&total_ring_bytes.to_le_bytes());
    gpadl_msg[24..28].copy_from_slice(&0u32.to_le_bytes()); // byte_offset = 0
    for i in 0..CHANNEL_TOTAL_PAGES {
        let off = 28 + i * 8;
        gpadl_msg[off..off + 8].copy_from_slice(&pfns[i].to_le_bytes());
    }

    if hv_post_message(msg_conn_id, &gpadl_msg).is_err() {
        crate::kprintln!("[CELEBRON-HV] Failed to send GPADL_HEADER for {}", label);
        return false;
    }

    // Wait for CHANNELMSG_GPADL_CREATED
    let mut resp = [0u8; 240];
    let mut gpadl_ok = false;
    for _ in 0..16 {
        let Some(len) = wait_synic_message(&mut resp, 4_000_000) else {
            break;
        };
        if len >= 20 {
            let mtype = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]);
            if mtype == CHANNELMSG_GPADL_CREATED {
                let status = u32::from_le_bytes([resp[16], resp[17], resp[18], resp[19]]);
                gpadl_ok = status == 0;
                break;
            }
        }
    }
    if !gpadl_ok {
        crate::kprintln!("[CELEBRON-HV] GPADL_CREATED failed for {}", label);
        return false;
    }

    // 3. Build & send CHANNELMSG_OPENCHANNEL (148 bytes)
    let mut open_msg = [0u8; 148];
    open_msg[0..4].copy_from_slice(&CHANNELMSG_OPENCHANNEL.to_le_bytes());
    open_msg[8..12].copy_from_slice(&ch.channel_id.to_le_bytes());
    open_msg[12..16].copy_from_slice(&((slot_idx as u32) + 1).to_le_bytes()); // open_id
    open_msg[16..20].copy_from_slice(&gpadl_id.to_le_bytes());
    open_msg[20..24].copy_from_slice(&0u32.to_le_bytes()); // target_vp = 0
    open_msg[24..28].copy_from_slice(&(RING_PAGES_PER_DIR as u32).to_le_bytes()); // downstream offset = 5

    if hv_post_message(msg_conn_id, &open_msg).is_err() {
        crate::kprintln!("[CELEBRON-HV] Failed to send OPENCHANNEL for {}", label);
        return false;
    }

    // Wait for CHANNELMSG_OPENCHANNEL_RESULT
    let mut open_ok = false;
    for _ in 0..16 {
        let Some(len) = wait_synic_message(&mut resp, 4_000_000) else {
            break;
        };
        if len >= 20 {
            let mtype = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]);
            if mtype == CHANNELMSG_OPENCHANNEL_RESULT {
                let status = u32::from_le_bytes([resp[16], resp[17], resp[18], resp[19]]);
                open_ok = status == 0;
                break;
            }
        }
    }

    if open_ok {
        ch.open = true;
        crate::kprintln!(
            "[CELEBRON-HV] Opened VMBus channel '{}' (relid={}, conn_id=0x{:X}, pipe={})",
            label,
            ch.channel_id,
            ch.connection_id,
            ch.is_pipe
        );
    } else {
        crate::kprintln!("[CELEBRON-HV] OPENCHANNEL_RESULT failed for {}", label);
    }
    open_ok
}

// ============================================================================
// Integration Component (IC) & Synthetic Keyboard Packet Handlers
// ============================================================================

/// Pick the best mutually supported IC version from host's offered list.
fn select_ic_version(offered: &[IcVersion], supported: &[(u16, u16)]) -> IcVersion {
    for &(smaj, smin) in supported {
        for v in offered {
            let maj = v.major;
            let min = v.minor;
            if maj == smaj && min == smin {
                return IcVersion {
                    major: smaj,
                    minor: smin,
                };
            }
        }
    }
    if !offered.is_empty() {
        offered[0]
    } else {
        IcVersion { major: 3, minor: 0 }
    }
}

/// Process pending packets on an Integration Service channel (Shutdown IC or Heartbeat IC).
unsafe fn poll_ic_channel(ch: &mut VmbusChannel, is_shutdown: bool) {
    if !ch.open {
        return;
    }
    let mut buf = [0u8; 2560];
    while let Some((raw_len, trans_id)) = ring_recv_packet(ch, &mut buf) {
        let ic_off = if ch.is_pipe { 8usize } else { 0usize };
        if raw_len < ic_off + 20 {
            continue;
        }

        let mut hdr = read_unaligned(buf.as_ptr().add(ic_off) as *const IcMsgHdr);
        let msg_type = hdr.message_type;

        match msg_type {
            ICMSGTYPE_NEGOTIATE => {
                let neg_off = ic_off + 20;
                if raw_len >= neg_off + 8 {
                    let fw_cnt =
                        u16::from_le_bytes([buf[neg_off], buf[neg_off + 1]]) as usize;
                    let msg_cnt =
                        u16::from_le_bytes([buf[neg_off + 2], buf[neg_off + 3]]) as usize;
                    let vers_off = neg_off + 8;

                    let mut fw_list = [IcVersion { major: 3, minor: 0 }; 8];
                    let mut msg_list = [IcVersion { major: 3, minor: 0 }; 8];
                    let fw_n = fw_cnt.min(8);
                    let msg_n = msg_cnt.min(8);

                    for i in 0..fw_n {
                        let p = vers_off + i * 4;
                        if p + 4 <= raw_len {
                            fw_list[i] = read_unaligned(buf.as_ptr().add(p) as *const IcVersion);
                        }
                    }
                    for i in 0..msg_n {
                        let p = vers_off + (fw_cnt + i) * 4;
                        if p + 4 <= raw_len {
                            msg_list[i] = read_unaligned(buf.as_ptr().add(p) as *const IcVersion);
                        }
                    }

                    let chosen_fw = select_ic_version(&fw_list[..fw_n], &[(3, 0), (1, 0)]);
                    let chosen_msg = if is_shutdown {
                        select_ic_version(&msg_list[..msg_n], &[(3, 2), (3, 1), (3, 0), (1, 0)])
                    } else {
                        select_ic_version(&msg_list[..msg_n], &[(3, 0), (1, 0)])
                    };

                    // Zero unused version slots and prepare Negotiate Response in-place
                    let mut reply_len = raw_len;
                    if ch.is_pipe {
                        let pipe_len =
                            u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
                        if pipe_len >= 28 && 8 + pipe_len <= raw_len {
                            reply_len = 8 + pipe_len;
                        }
                    }

                    buf[neg_off..neg_off + 2].copy_from_slice(&1u16.to_le_bytes());
                    buf[neg_off + 2..neg_off + 4].copy_from_slice(&1u16.to_le_bytes());
                    for b in &mut buf[vers_off..reply_len] {
                        *b = 0;
                    }
                    write_unaligned(buf.as_mut_ptr().add(vers_off) as *mut IcVersion, chosen_fw);
                    write_unaligned(
                        buf.as_mut_ptr().add(vers_off + 4) as *mut IcVersion,
                        chosen_msg,
                    );

                    hdr.status = 0; // HV_S_OK
                    hdr.flags = ICMSGHDRFLAG_TRANSACTION | ICMSGHDRFLAG_RESPONSE;
                    write_unaligned(buf.as_mut_ptr().add(ic_off) as *mut IcMsgHdr, hdr);

                    ring_send_packet(ch, &buf[..reply_len], trans_id, false);
                    ch.negotiated = true;
                    let fw_maj = chosen_fw.major;
                    let fw_min = chosen_fw.minor;
                    let msg_maj = chosen_msg.major;
                    let msg_min = chosen_msg.minor;
                    crate::kprintln!(
                        "[CELEBRON-HV] {} IC negotiated OK (fw={}.{}, msg={}.{})",
                        if is_shutdown { "Shutdown" } else { "Heartbeat" },
                        fw_maj,
                        fw_min,
                        msg_maj,
                        msg_min
                    );
                }
            }
            ICMSGTYPE_HEARTBEAT if !is_shutdown => {
                let mut reply_len = raw_len;
                if ch.is_pipe {
                    let pipe_len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
                    if pipe_len >= 20 && 8 + pipe_len <= raw_len {
                        reply_len = 8 + pipe_len;
                    }
                }
                let data_off = ic_off + 20;
                if reply_len >= data_off + 12 {
                    let seq = read_unaligned(buf.as_ptr().add(data_off) as *const u64);
                    let next_seq = seq.wrapping_add(1);
                    write_unaligned(buf.as_mut_ptr().add(data_off) as *mut u64, next_seq);
                    // ApplicationState::HEALTHY = 1
                    write_unaligned(buf.as_mut_ptr().add(data_off + 8) as *mut u32, 1u32);
                }
                hdr.status = 0; // HV_S_OK
                hdr.flags = ICMSGHDRFLAG_TRANSACTION | ICMSGHDRFLAG_RESPONSE;
                write_unaligned(buf.as_mut_ptr().add(ic_off) as *mut IcMsgHdr, hdr);
                ring_send_packet(ch, &buf[..reply_len], trans_id, false);
                HEARTBEAT_COUNT.fetch_add(1, Ordering::Relaxed);
            }
            ICMSGTYPE_SHUTDOWN if is_shutdown => {
                let mut reply_len = raw_len;
                if ch.is_pipe {
                    let pipe_len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
                    if pipe_len >= 20 && 8 + pipe_len <= raw_len {
                        reply_len = 8 + pipe_len;
                    }
                }
                let data_off = ic_off + 20;
                let mut is_restart = false;
                if reply_len >= data_off + 12 {
                    let shutdown_flags =
                        read_unaligned(buf.as_ptr().add(data_off + 8) as *const u32);
                    // Bit 1 = Restart flag in Hyper-V ShutdownFlags
                    is_restart = (shutdown_flags & 0x02) != 0;
                }

                // Acknowledge Hyper-V host with HV_S_OK BEFORE powering off!
                hdr.status = 0; // HV_S_OK
                hdr.flags = ICMSGHDRFLAG_TRANSACTION | ICMSGHDRFLAG_RESPONSE;
                write_unaligned(buf.as_mut_ptr().add(ic_off) as *mut IcMsgHdr, hdr);
                ring_send_packet(ch, &buf[..reply_len], trans_id, false);

                crate::kprintln!(
                    "[CELEBRON-HV] Received Hyper-V Host {} IC request -> acknowledged HV_S_OK!",
                    if is_restart { "RESTART" } else { "SHUTDOWN" }
                );

                // Brief delay so Hyper-V host processes our HV_S_OK response cleanly
                for _ in 0..8_000_000u64 {
                    core::hint::spin_loop();
                }

                power::signal_power_action(if is_restart {
                    PowerAction::RebootCold
                } else {
                    PowerAction::Shutdown
                });
            }
            _ => {}
        }
    }
}

/// Process pending packets on the Hyper-V Synthetic Keyboard channel.
unsafe fn poll_keyboard_channel(ch: &mut VmbusChannel) {
    if !ch.open {
        return;
    }
    let mut buf = [0u8; 256];
    while let Some((raw_len, _trans_id)) = ring_recv_packet(ch, &mut buf) {
        let kbd_off = if ch.is_pipe { 8usize } else { 0usize };
        if raw_len < kbd_off + 8 {
            continue;
        }
        let msg_type = read_unaligned(buf.as_ptr().add(kbd_off) as *const u32);
        match msg_type {
            SYNTH_KBD_PROTOCOL_RESPONSE => {
                let accepted = read_unaligned(buf.as_ptr().add(kbd_off + 4) as *const u32);
                if accepted != 0 {
                    ch.negotiated = true;
                    crate::kprintln!("[CELEBRON-HV] Synthetic Keyboard protocol accepted by host!");
                }
            }
            SYNTH_KBD_EVENT => {
                if raw_len >= kbd_off + 12 {
                    let make_code = read_unaligned(buf.as_ptr().add(kbd_off + 4) as *const u16);
                    let flags = read_unaligned(buf.as_ptr().add(kbd_off + 8) as *const u32);
                    KBD_EVENT_COUNT.fetch_add(1, Ordering::Relaxed);
                    // Trigger on key press (break flag == 0)
                    if (flags & KEYSTROKE_IS_BREAK) == 0 {
                        match make_code {
                            0x1F => {
                                crate::kprintln!("[CELEBRON-HV] Synthetic Keyboard: [S] -> SHUTDOWN");
                                power::signal_power_action(PowerAction::Shutdown);
                            }
                            0x13 => {
                                crate::kprintln!("[CELEBRON-HV] Synthetic Keyboard: [R] -> COLD REBOOT");
                                power::signal_power_action(PowerAction::RebootCold);
                            }
                            0x11 => {
                                crate::kprintln!("[CELEBRON-HV] Synthetic Keyboard: [W] -> WARM REBOOT");
                                power::signal_power_action(PowerAction::RebootWarm);
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Called from `idt.rs` when Hyper-V SynIC delivers Vector 48 (`SINT2`).
#[inline(always)]
pub fn on_synic_interrupt() {
    SYNIC_IRQ_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Clear the SynIC Event Flag bit for `channel_id` in `SIEFP[2]` so Hyper-V delivers
/// edge-triggered SINT2 interrupts for subsequent packets on this channel.
#[inline(always)]
unsafe fn clear_siefp_bit(siefp_page: u64, channel_id: u32) {
    let bit_idx = channel_id as usize;
    if siefp_page == 0 || bit_idx >= 2048 {
        return;
    }
    let slot_base = siefp_page + (VMBUS_SINT_INDEX as u64) * 256;
    let word = &*((slot_base as *const AtomicU64).add(bit_idx / 64));
    word.fetch_and(!(1u64 << (bit_idx % 64)), Ordering::AcqRel);
}

/// Poll all active Hyper-V VMBus services (Shutdown IC, Heartbeat IC, Synthetic Keyboard)
/// and drain any pending SynIC control messages.
pub fn poll_services() {
    unsafe {
        let st = &mut *addr_of_mut!(HV_STATE);
        if !st.active {
            return;
        }

        // Drain any asynchronous SynIC control messages so SIMP[2] never stalls
        let mut tmp = [0u8; 240];
        while try_recv_synic_message(&mut tmp).is_some() {}

        let siefp = st.siefp_page;
        if st.shutdown_ch.open {
            clear_siefp_bit(siefp, st.shutdown_ch.channel_id);
        }
        if st.heartbeat_ch.open {
            clear_siefp_bit(siefp, st.heartbeat_ch.channel_id);
        }
        if st.keyboard_ch.open {
            clear_siefp_bit(siefp, st.keyboard_ch.channel_id);
        }

        poll_ic_channel(&mut st.shutdown_ch, true);
        poll_ic_channel(&mut st.heartbeat_ch, false);
        poll_keyboard_channel(&mut st.keyboard_ch);
    }
}

/// Initialize Hyper-V Hypercall Page, SynIC, VMBus connection, and open Shutdown IC,
/// Heartbeat IC, and Synthetic Keyboard channels.
pub fn init(
    is_hyperv: bool,
    pmm: &mut PhysicalMemoryManager,
    vmm: &mut VirtualMemoryManager,
) -> HyperVStatus {
    if !is_hyperv {
        return HyperVStatus::offline();
    }

    unsafe {
        // 1. Identify Celebron OS to Hyper-V (`HV_X64_MSR_GUEST_OS_ID`)
        wrmsr(HV_X64_MSR_GUEST_OS_ID, CELEBRON_GUEST_OS_ID);

        // 2. Allocate & enable Hypercall Page (`HV_X64_MSR_HYPERCALL`)
        let Some(hc_page) = pmm.alloc_frame() else {
            return HyperVStatus::offline();
        };
        let hc_cur = rdmsr(HV_X64_MSR_HYPERCALL);
        wrmsr(HV_X64_MSR_HYPERCALL, (hc_page & !0xFFFu64) | (hc_cur & 0xFFEu64) | 1);
        let hc_verify = rdmsr(HV_X64_MSR_HYPERCALL);
        if (hc_verify & 1) == 0 {
            crate::kprintln!("[CELEBRON-HV] Failed to enable Hypercall page");
            return HyperVStatus::offline();
        }

        // 3. Allocate PostMessage page + SynIC Message/Event pages + VMBus global pages
        let Some(post_msg_page) = pmm.alloc_frame() else {
            return HyperVStatus::offline();
        };
        let Some(simp_page) = pmm.alloc_frame() else {
            return HyperVStatus::offline();
        };
        let Some(siefp_page) = pmm.alloc_frame() else {
            return HyperVStatus::offline();
        };
        let Some(int_page) = pmm.alloc_frame() else {
            return HyperVStatus::offline();
        };
        let Some(mon1_page) = pmm.alloc_frame() else {
            return HyperVStatus::offline();
        };
        let Some(mon2_page) = pmm.alloc_frame() else {
            return HyperVStatus::offline();
        };

        let st = &mut *addr_of_mut!(HV_STATE);
        st.hypercall_page = hc_page;
        st.post_msg_page = post_msg_page;
        st.simp_page = simp_page;
        st.siefp_page = siefp_page;

        // 4. Configure SynIC (SIMP, SIEFP, SINT2 -> Vector 48, SCONTROL enable)
        let simp_cur = rdmsr(HV_X64_MSR_SIMP);
        wrmsr(HV_X64_MSR_SIMP, (simp_page & !0xFFFu64) | (simp_cur & 0xFFEu64) | 1);

        let siefp_cur = rdmsr(HV_X64_MSR_SIEFP);
        wrmsr(HV_X64_MSR_SIEFP, (siefp_page & !0xFFFu64) | (siefp_cur & 0xFFEu64) | 1);

        // SINT2: vector = HYPERV_VMBUS_VECTOR (48), unmasked (bit 16 = 0), manual EOI (bit 17 = 0)
        let sint2_cur = rdmsr(HV_X64_MSR_SINT2);
        let sint2_new = (sint2_cur & !0x0007_00FFu64) | (HYPERV_VMBUS_VECTOR as u64);
        wrmsr(HV_X64_MSR_SINT2, sint2_new);

        let scontrol_cur = rdmsr(HV_X64_MSR_SCONTROL);
        wrmsr(HV_X64_MSR_SCONTROL, scontrol_cur | 1);

        // 5. Negotiate VMBus Version (`CHANNELMSG_INITIATE_CONTACT`)
        let candidates: [(u32, u32, u64); 3] = [
            (VMBUS_VERSION_WIN10_V5_1, VMBUS_CONN_ID_WIN10, VMBUS_SINT_INDEX as u64),
            (VMBUS_VERSION_WIN10, VMBUS_CONN_ID_LEGACY, int_page),
            (VMBUS_VERSION_WIN8_1, VMBUS_CONN_ID_LEGACY, int_page),
        ];

        let mut connected = false;
        let mut resp = [0u8; 240];

        for &(ver, conn_id, target_or_int) in &candidates {
            let mut contact = [0u8; 40];
            contact[0..4].copy_from_slice(&CHANNELMSG_INITIATE_CONTACT.to_le_bytes());
            contact[8..12].copy_from_slice(&ver.to_le_bytes());
            contact[12..16].copy_from_slice(&0u32.to_le_bytes()); // target_vcpu = 0
            contact[16..24].copy_from_slice(&target_or_int.to_le_bytes());
            contact[24..32].copy_from_slice(&mon1_page.to_le_bytes());
            contact[32..40].copy_from_slice(&mon2_page.to_le_bytes());

            if hv_post_message(conn_id, &contact).is_err() {
                continue;
            }

            if let Some(len) = wait_synic_message(&mut resp, 8_000_000) {
                if len >= 16 {
                    let mtype = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]);
                    let supported = resp[8];
                    if mtype == CHANNELMSG_VERSION_RESPONSE && supported != 0 {
                        let host_conn_id =
                            u32::from_le_bytes([resp[12], resp[13], resp[14], resp[15]]);
                        st.negotiated_version = ver;
                        st.msg_conn_id = if ver >= (5 << 16)
                            && host_conn_id != 0
                            && host_conn_id < 0x10000
                        {
                            host_conn_id
                        } else {
                            conn_id
                        };
                        connected = true;
                        break;
                    }
                }
            }
        }

        if !connected {
            crate::kprintln!("[CELEBRON-HV] VMBus InitiateContact did not receive VersionResponse");
            return HyperVStatus::offline();
        }

        crate::kprintln!(
            "[CELEBRON-HV] VMBus connected! version={}.{} (conn_id={})",
            st.negotiated_version >> 16,
            st.negotiated_version & 0xFFFF,
            st.msg_conn_id
        );

        // 6. Request all VMBus device offers (`CHANNELMSG_REQUESTOFFERS`)
        let mut req_offers = [0u8; 8];
        req_offers[0..4].copy_from_slice(&CHANNELMSG_REQUESTOFFERS.to_le_bytes());
        if hv_post_message(st.msg_conn_id, &req_offers).is_err() {
            crate::kprintln!("[CELEBRON-HV] Failed to post CHANNELMSG_REQUESTOFFERS");
            return HyperVStatus::offline();
        }

        for _ in 0..64 {
            let Some(len) = wait_synic_message(&mut resp, 6_000_000) else {
                break;
            };
            if len < 8 {
                continue;
            }
            let mtype = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]);
            if mtype == CHANNELMSG_ALLOFFERS_DELIVERED {
                break;
            }
            if mtype == CHANNELMSG_OFFERCHANNEL && len >= 196 {
                st.total_offers += 1;
                let mut if_guid = [0u8; 16];
                if_guid.copy_from_slice(&resp[8..24]);
                let flags = u16::from_le_bytes([resp[56], resp[57]]);
                let channel_id = u32::from_le_bytes([resp[184], resp[185], resp[186], resp[187]]);
                let raw_conn_id =
                    u32::from_le_bytes([resp[192], resp[193], resp[194], resp[195]]);
                let connection_id = if raw_conn_id != 0 { raw_conn_id } else { 2 };
                let is_pipe = (flags & VMBUS_CHANNEL_NAMED_PIPE_MODE) != 0;

                if if_guid == GUID_SHUTDOWN_IC {
                    st.shutdown_ch.offered = true;
                    st.shutdown_ch.is_pipe = is_pipe;
                    st.shutdown_ch.channel_id = channel_id;
                    st.shutdown_ch.connection_id = connection_id;
                } else if if_guid == GUID_HEARTBEAT_IC {
                    st.heartbeat_ch.offered = true;
                    st.heartbeat_ch.is_pipe = is_pipe;
                    st.heartbeat_ch.channel_id = channel_id;
                    st.heartbeat_ch.connection_id = connection_id;
                } else if if_guid == GUID_SYNTH_KEYBOARD {
                    st.keyboard_ch.offered = true;
                    st.keyboard_ch.is_pipe = is_pipe;
                    st.keyboard_ch.channel_id = channel_id;
                    st.keyboard_ch.connection_id = connection_id;
                }
            }
        }

        crate::kprintln!(
            "[CELEBRON-HV] Discovered {} VMBus offers (Shutdown={}, Heartbeat={}, SynthKbd={})",
            st.total_offers,
            st.shutdown_ch.offered,
            st.heartbeat_ch.offered,
            st.keyboard_ch.offered
        );

        // 7. Open Shutdown IC, Heartbeat IC, and Synthetic Keyboard channels
        let mut shutdown_ch = st.shutdown_ch;
        let mut heartbeat_ch = st.heartbeat_ch;
        let mut keyboard_ch = st.keyboard_ch;

        open_vmbus_channel(0, &mut shutdown_ch, pmm, vmm, "Shutdown-IC");
        open_vmbus_channel(1, &mut heartbeat_ch, pmm, vmm, "Heartbeat-IC");
        open_vmbus_channel(2, &mut keyboard_ch, pmm, vmm, "Synth-Keyboard");

        st.shutdown_ch = shutdown_ch;
        st.heartbeat_ch = heartbeat_ch;
        st.keyboard_ch = keyboard_ch;

        // 8. Initiate Synthetic Keyboard Protocol Handshake (Guest -> Host)
        if st.keyboard_ch.open {
            let mut kbd_req = [0u8; 8];
            kbd_req[0..4].copy_from_slice(&SYNTH_KBD_PROTOCOL_REQUEST.to_le_bytes());
            kbd_req[4..8].copy_from_slice(&SYNTH_KBD_VERSION_WIN8.to_le_bytes());
            ring_send_packet(&st.keyboard_ch, &kbd_req, 1, true);
        }

        st.active = true;

        // 9. Run an initial short negotiation poll so Shutdown IC & Heartbeat IC complete
        //    ICMSGTYPE_NEGOTIATE immediately during boot!
        for _ in 0..400_000u64 {
            poll_services();
            if (!st.shutdown_ch.open || st.shutdown_ch.negotiated)
                && (!st.heartbeat_ch.open || st.heartbeat_ch.negotiated)
                && (!st.keyboard_ch.open || st.keyboard_ch.negotiated)
            {
                break;
            }
            core::hint::spin_loop();
        }

        HyperVStatus {
            vmbus_online: true,
            vmbus_version: st.negotiated_version,
            offers_discovered: st.total_offers,
            shutdown_ic_ready: st.shutdown_ch.negotiated || st.shutdown_ch.open,
            heartbeat_ic_ready: st.heartbeat_ch.negotiated || st.heartbeat_ch.open,
            synth_kbd_ready: st.keyboard_ch.negotiated || st.keyboard_ch.open,
        }
    }
}

#[allow(dead_code)]
pub fn heartbeat_count() -> u64 {
    HEARTBEAT_COUNT.load(Ordering::Relaxed)
}

#[allow(dead_code)]
pub fn kbd_event_count() -> u64 {
    KBD_EVENT_COUNT.load(Ordering::Relaxed)
}
