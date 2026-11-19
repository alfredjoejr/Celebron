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
const VM_PKT_DATA_USING_GPA_DIRECT: u16 = 9;
const VM_PKT_COMP: u16 = 11;
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

// StorVSP (Synthetic SCSI) Protocol Constants
const VSTOR_OP_COMPLETE_IO: u32 = 1;
const VSTOR_OP_EXECUTE_SRB: u32 = 3;
const VSTOR_OP_BEGIN_INIT: u32 = 7;
const VSTOR_OP_END_INIT: u32 = 8;
const VSTOR_OP_QUERY_PROTOCOL_VERSION: u32 = 9;
const VSTOR_OP_QUERY_PROPERTIES: u32 = 10;

const VSTOR_VERSION_THRESHOLD: u16 = 0x0602;
const VSTOR_VERSION_BLUE: u16 = 0x0600;
const VSTOR_VERSION_WIN8: u16 = 0x0501;

const VSTOR_PACKET_SIZE: usize = 64;
const VSTOR_FLAG_REQUEST_COMPLETION: u32 = 1;

const SCSI_IOCTL_DATA_OUT: u8 = 0;
const SCSI_IOCTL_DATA_IN: u8 = 1;
const SCSI_IOCTL_DATA_UNSPECIFIED: u8 = 2;

const SRB_STATUS_MASK: u8 = 0x3F;
const SRB_STATUS_SUCCESS: u8 = 0x01;
const SRB_FLAGS_DATA_IN: u32 = 0x0000_0040;
const SRB_FLAGS_DATA_OUT: u32 = 0x0000_0080;

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
/// Synthetic SCSI Controller (StorVSC): `{ba6163d9-04a1-4d29-b605-72e2ffb1dc7f}`
const GUID_SCSI_STORVSC: [u8; 16] = [
    0xd9, 0x63, 0x61, 0xba, 0xa1, 0x04, 0x29, 0x4d, 0xb6, 0x05, 0x72, 0xe2, 0xff, 0xb1, 0xdc, 0x7f,
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

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct GpaDirectHeader {
    reserved: u32,
    range_count: u32,
}

const _: () = assert!(core::mem::size_of::<GpaDirectHeader>() == 8);

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct GpaRange {
    byte_count: u32,
    byte_offset: u32,
    pfn: u64,
}

const _: () = assert!(core::mem::size_of::<GpaRange>() == 16);

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct ScsiRequest {
    length: u16,
    srb_status: u8,
    scsi_status: u8,
    reserved1: u8,
    path_id: u8,
    target_id: u8,
    lun: u8,
    cdb_length: u8,
    sense_info_ex_length: u8,
    data_in: u8,
    properties: u8,
    data_transfer_length: u32,
    payload: [u8; 20],
    reserve: u16,
    queue_tag: u8,
    queue_action: u8,
    srb_flags: u32,
    time_out_value: u32,
    queue_sort_key: u32,
}

const _: () = assert!(core::mem::size_of::<ScsiRequest>() == 52);

impl ScsiRequest {
    const fn zeroed() -> Self {
        Self {
            length: 0,
            srb_status: 0,
            scsi_status: 0,
            reserved1: 0,
            path_id: 0,
            target_id: 0,
            lun: 0,
            cdb_length: 0,
            sense_info_ex_length: 0,
            data_in: 0,
            properties: 0,
            data_transfer_length: 0,
            payload: [0; 20],
            reserve: 0,
            queue_tag: 0,
            queue_action: 0,
            srb_flags: 0,
            time_out_value: 0,
            queue_sort_key: 0,
        }
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct VstorPacket {
    operation: u32,
    flags: u32,
    status: u32,
    body: ScsiRequest,
}

const _: () = assert!(core::mem::size_of::<VstorPacket>() == VSTOR_PACKET_SIZE);

impl VstorPacket {
    const fn new(operation: u32, flags: u32) -> Self {
        Self {
            operation,
            flags,
            status: 0,
            body: ScsiRequest::zeroed(),
        }
    }

    unsafe fn as_bytes(&self) -> &[u8; VSTOR_PACKET_SIZE] {
        &*(self as *const Self as *const [u8; VSTOR_PACKET_SIZE])
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StorVscDiskInfo {
    pub online: bool,
    pub lun: u8,
    pub sector_size: u32,
    pub sector_count: u64,
    #[allow(dead_code)]
    pub proto_version: u16,
}

impl StorVscDiskInfo {
    pub const fn offline() -> Self {
        Self {
            online: false,
            lun: 0,
            sector_size: 512,
            sector_count: 0,
            proto_version: 0,
        }
    }
}

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
    storvsc_ch: VmbusChannel,
    stor_dma_page: u64,
    stor_tx_seq: u64,
    stor_disk: StorVscDiskInfo,
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
    storvsc_ch: VmbusChannel::empty(),
    stor_dma_page: 0,
    stor_tx_seq: 1,
    stor_disk: StorVscDiskInfo::offline(),
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
    pub storvsc_disk: StorVscDiskInfo,
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
            storvsc_disk: StorVscDiskInfo::offline(),
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
/// Returns `Some((packet_type, payload_len, transaction_id))`.
unsafe fn ring_recv_packet_typed(
    ch: &VmbusChannel,
    out_payload: &mut [u8],
) -> Option<(u16, usize, u64)> {
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

    Some((desc.packet_type, copy_len, desc.transaction_id))
}

/// Read one packet from a channel's Host->Guest Receive Ring Buffer.
/// Returns `Some((payload_len, transaction_id))`.
unsafe fn ring_recv_packet(ch: &VmbusChannel, out_payload: &mut [u8]) -> Option<(usize, u64)> {
    ring_recv_packet_typed(ch, out_payload).map(|(_, len, tx)| (len, tx))
}

/// Common writer helper for in-band and GPA-direct VMBus ring packets.
unsafe fn ring_write_frame(
    ch: &VmbusChannel,
    packet_type: u16,
    header_extra: &[u8],
    payload: &[u8],
    trans_id: u64,
    req_completion: bool,
) -> bool {
    if !ch.open || ch.send_ring_virt == 0 {
        return false;
    }

    let hdr_total = 16 + header_extra.len();
    let padded_payload_len = (payload.len() + 7) & !7;
    let msg_len = hdr_total + padded_payload_len;
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
        packet_type,
        data_offset8: (hdr_total / 8) as u16,
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
    if !header_extra.is_empty() {
        ring_copy_to(ch.send_ring_virt, old_write_idx + 16, header_extra);
    }
    ring_copy_to(ch.send_ring_virt, old_write_idx + hdr_total, payload);

    let pad_bytes = padded_payload_len - payload.len();
    if pad_bytes > 0 {
        ring_zero(
            ch.send_ring_virt,
            old_write_idx + hdr_total + payload.len(),
            pad_bytes,
        );
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

/// Write one in-band packet to a channel's Guest->Host Send Ring Buffer and signal host.
unsafe fn ring_send_packet(
    ch: &VmbusChannel,
    payload: &[u8],
    trans_id: u64,
    req_completion: bool,
) -> bool {
    ring_write_frame(ch, VM_PKT_DATA_INBAND, &[], payload, trans_id, req_completion)
}

/// Write one single-range GPA-Direct packet (`VM_PKT_DATA_USING_GPA_DIRECT`) and signal host.
unsafe fn ring_send_gpa_direct_packet(
    ch: &VmbusChannel,
    gpa_phys: u64,
    byte_count: u32,
    payload: &[u8],
    trans_id: u64,
) -> bool {
    let hdr = GpaDirectHeader {
        reserved: 0,
        range_count: 1,
    };
    let range = GpaRange {
        byte_count,
        byte_offset: 0,
        pfn: gpa_phys / PAGE_SIZE,
    };
    let mut extra = [0u8; 24];
    let hdr_bytes =
        core::slice::from_raw_parts((&hdr as *const GpaDirectHeader) as *const u8, 8);
    let rng_bytes = core::slice::from_raw_parts((&range as *const GpaRange) as *const u8, 16);
    extra[0..8].copy_from_slice(hdr_bytes);
    extra[8..24].copy_from_slice(rng_bytes);

    ring_write_frame(
        ch,
        VM_PKT_DATA_USING_GPA_DIRECT,
        &extra,
        payload,
        trans_id,
        true,
    )
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

// ============================================================================
// Hyper-V VMBus Synthetic SCSI (`StorVSC`) Block Driver
// ============================================================================

unsafe fn storvsc_next_tx() -> u64 {
    let st = &mut *addr_of_mut!(HV_STATE);
    let tx = st.stor_tx_seq;
    st.stor_tx_seq = st.stor_tx_seq.wrapping_add(1);
    tx
}

/// Poll the StorVSC receive ring buffer until a `VM_PKT_COMP` matches `expected_tx`.
unsafe fn storvsc_wait_completion(expected_tx: u64, out_buf: &mut [u8; VSTOR_PACKET_SIZE]) -> bool {
    let st = &*addr_of_mut!(HV_STATE);
    let mut tmp = [0u8; 128];

    for _ in 0..12_000_000u64 {
        clear_siefp_bit(st.siefp_page, st.storvsc_ch.channel_id);
        if let Some((pkt_type, len, tx_id)) = ring_recv_packet_typed(&st.storvsc_ch, &mut tmp) {
            if pkt_type == VM_PKT_COMP && tx_id == expected_tx && len >= 12 {
                let copy_len = len.min(VSTOR_PACKET_SIZE);
                out_buf.fill(0);
                out_buf[..copy_len].copy_from_slice(&tmp[..copy_len]);
                return true;
            }
        }
        core::hint::spin_loop();
    }
    false
}

/// Issue a synchronous StorVSP control request (`BEGIN_INIT`, `QUERY_PROTOCOL_VERSION`,
/// `QUERY_PROPERTIES`, or `END_INIT`) and return its `NtStatus`.
unsafe fn storvsc_send_ctl(op: u32, proto_ver: Option<u16>) -> Option<u32> {
    let st = &*addr_of_mut!(HV_STATE);
    let mut pkt = VstorPacket::new(op, VSTOR_FLAG_REQUEST_COMPLETION);
    if let Some(ver) = proto_ver {
        // `ProtocolVersion { major_minor: u16, reserved: u16 }` overlays bytes 12..16.
        pkt.body.length = ver;
    }

    let tx = storvsc_next_tx();
    if !ring_send_packet(&st.storvsc_ch, pkt.as_bytes(), tx, true) {
        return None;
    }

    let mut resp = [0u8; VSTOR_PACKET_SIZE];
    if !storvsc_wait_completion(tx, &mut resp) {
        return None;
    }
    Some(u32::from_le_bytes([resp[8], resp[9], resp[10], resp[11]]))
}

/// Issue a single synchronous SCSI SRB via `VSTOR_OP_EXECUTE_SRB`.
unsafe fn storvsc_exec_srb(
    lun: u8,
    cdb: &[u8],
    data_in: u8,
    xfer_len: u32,
    use_dma: bool,
) -> Result<(), &'static str> {
    let st = &*addr_of_mut!(HV_STATE);
    if !st.storvsc_ch.open || cdb.len() > 16 {
        return Err("StorVSC channel not open or invalid CDB");
    }

    let mut pkt = VstorPacket::new(VSTOR_OP_EXECUTE_SRB, VSTOR_FLAG_REQUEST_COMPLETION);
    pkt.body.length = core::mem::size_of::<ScsiRequest>() as u16; // 0x34 (52 bytes)
    pkt.body.path_id = 0;
    pkt.body.target_id = 0;
    pkt.body.lun = lun;
    pkt.body.cdb_length = cdb.len() as u8;
    pkt.body.sense_info_ex_length = 0x14;
    pkt.body.data_in = data_in;
    pkt.body.data_transfer_length = xfer_len;
    pkt.body.payload[..cdb.len()].copy_from_slice(cdb);
    pkt.body.srb_flags = match data_in {
        SCSI_IOCTL_DATA_IN => SRB_FLAGS_DATA_IN,
        SCSI_IOCTL_DATA_OUT => SRB_FLAGS_DATA_OUT,
        _ => 0,
    };
    pkt.body.time_out_value = 60;

    let tx = storvsc_next_tx();
    let sent = if use_dma && xfer_len > 0 {
        ring_send_gpa_direct_packet(&st.storvsc_ch, st.stor_dma_page, xfer_len, pkt.as_bytes(), tx)
    } else {
        ring_send_packet(&st.storvsc_ch, pkt.as_bytes(), tx, true)
    };
    if !sent {
        return Err("StorVSC ring send failed");
    }

    let mut resp = [0u8; VSTOR_PACKET_SIZE];
    if !storvsc_wait_completion(tx, &mut resp) {
        return Err("StorVSC completion timeout");
    }

    let op = u32::from_le_bytes([resp[0], resp[1], resp[2], resp[3]]);
    let nt_status = u32::from_le_bytes([resp[8], resp[9], resp[10], resp[11]]);
    let srb_status = resp[14];
    let scsi_status = resp[15];

    if op != VSTOR_OP_COMPLETE_IO || nt_status != 0 {
        return Err("StorVSC packet failed");
    }
    if (srb_status & SRB_STATUS_MASK) != SRB_STATUS_SUCCESS || scsi_status != 0 {
        return Err("StorVSC SCSI SRB status error");
    }
    Ok(())
}

/// Run the StorVSP initialization sequence and probe LUNs `0..4` via `READ CAPACITY(10)`.
unsafe fn storvsc_handshake_and_probe() -> StorVscDiskInfo {
    let st = &*addr_of_mut!(HV_STATE);
    if !st.storvsc_ch.open || st.stor_dma_page == 0 {
        return StorVscDiskInfo::offline();
    }

    // Step A: BEGIN_INITIALIZATION
    if storvsc_send_ctl(VSTOR_OP_BEGIN_INIT, None) != Some(0) {
        crate::kprintln!("[CELEBRON-HV] StorVSC BEGIN_INITIALIZATION failed");
        return StorVscDiskInfo::offline();
    }

    // Step B: QUERY_PROTOCOL_VERSION
    let mut negotiated_proto = 0u16;
    for &ver in &[
        VSTOR_VERSION_THRESHOLD,
        VSTOR_VERSION_BLUE,
        VSTOR_VERSION_WIN8,
    ] {
        if storvsc_send_ctl(VSTOR_OP_QUERY_PROTOCOL_VERSION, Some(ver)) == Some(0) {
            negotiated_proto = ver;
            break;
        }
    }
    if negotiated_proto == 0 {
        crate::kprintln!("[CELEBRON-HV] StorVSC QUERY_PROTOCOL_VERSION failed");
        return StorVscDiskInfo::offline();
    }

    // Step C: QUERY_PROPERTIES
    if storvsc_send_ctl(VSTOR_OP_QUERY_PROPERTIES, None) != Some(0) {
        crate::kprintln!("[CELEBRON-HV] StorVSC QUERY_PROPERTIES failed");
        return StorVscDiskInfo::offline();
    }

    // Step D: END_INITIALIZATION
    if storvsc_send_ctl(VSTOR_OP_END_INIT, None) != Some(0) {
        crate::kprintln!("[CELEBRON-HV] StorVSC END_INITIALIZATION failed");
        return StorVscDiskInfo::offline();
    }

    // Step E: Scan LUN 0..4 with TEST UNIT READY + READ CAPACITY(10)
    let tur_cdb = [0u8; 6];
    let cap_cdb = [0x25u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];

    for lun in 0u8..4 {
        // Clear any initial UNIT ATTENTION before issuing READ CAPACITY(10)
        for _ in 0..2 {
            let _ = storvsc_exec_srb(lun, &tur_cdb, SCSI_IOCTL_DATA_UNSPECIFIED, 0, false);
        }

        core::ptr::write_bytes(st.stor_dma_page as *mut u8, 0, 8);
        fence(Ordering::SeqCst);

        if storvsc_exec_srb(lun, &cap_cdb, SCSI_IOCTL_DATA_IN, 8, true).is_ok() {
            fence(Ordering::Acquire);
            let b = core::slice::from_raw_parts(st.stor_dma_page as *const u8, 8);
            let last_lba = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
            let blk_len = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
            if blk_len == 512 && last_lba > 0 {
                let sector_count = (last_lba as u64) + 1;
                crate::kprintln!(
                    "[CELEBRON-HV] StorVSC SCSI LUN {} online: {} sectors x {} B ({} MiB, proto=0x{:04X})",
                    lun,
                    sector_count,
                    blk_len,
                    (sector_count * (blk_len as u64)) / (1024 * 1024),
                    negotiated_proto
                );
                return StorVscDiskInfo {
                    online: true,
                    lun,
                    sector_size: blk_len,
                    sector_count,
                    proto_version: negotiated_proto,
                };
            }
        }
    }

    crate::kprintln!("[CELEBRON-HV] StorVSC handshake OK but no 512-byte LUN found");
    StorVscDiskInfo::offline()
}

/// Shared chunked SCSI READ(10)/WRITE(10) transfer engine capped at 8 sectors (4 KiB) per SRB.
unsafe fn storvsc_transfer(
    lba: u64,
    count: u16,
    is_write: bool,
    read_dst: Option<&mut [u8]>,
    write_src: Option<&[u8]>,
) -> Result<(), &'static str> {
    let st = &*addr_of_mut!(HV_STATE);
    if !st.stor_disk.online || st.stor_dma_page == 0 {
        return Err("StorVSC disk offline");
    }
    if count == 0 {
        return Ok(());
    }
    let total_bytes = (count as usize) * 512;
    let end_lba = lba
        .checked_add(count as u64)
        .ok_or("StorVSC LBA overflow")?;
    if end_lba > st.stor_disk.sector_count || end_lba > u32::MAX as u64 {
        return Err("StorVSC LBA out of bounds");
    }
    if let Some(ref dst) = read_dst {
        if dst.len() < total_bytes {
            return Err("StorVSC read buffer too small");
        }
    }
    if let Some(src) = write_src {
        if src.len() < total_bytes {
            return Err("StorVSC write buffer too small");
        }
    }

    let mut read_dst = read_dst;
    let mut cur_lba = lba;
    let mut rem = count;
    let mut byte_off = 0usize;

    while rem > 0 {
        let chunk = rem.min(8);
        let chunk_bytes = (chunk as usize) * 512;
        let lba_be = (cur_lba as u32).to_be_bytes();
        let cnt_be = chunk.to_be_bytes();
        let opcode = if is_write { 0x2Au8 } else { 0x28u8 };
        let cdb = [
            opcode, 0, lba_be[0], lba_be[1], lba_be[2], lba_be[3], 0, cnt_be[0], cnt_be[1], 0,
        ];

        if is_write {
            let src = write_src.ok_or("Missing write buffer")?;
            core::ptr::copy_nonoverlapping(
                src.as_ptr().add(byte_off),
                st.stor_dma_page as *mut u8,
                chunk_bytes,
            );
            fence(Ordering::Release);
            storvsc_exec_srb(
                st.stor_disk.lun,
                &cdb,
                SCSI_IOCTL_DATA_OUT,
                chunk_bytes as u32,
                true,
            )?;
        } else {
            core::ptr::write_bytes(st.stor_dma_page as *mut u8, 0, chunk_bytes);
            fence(Ordering::SeqCst);
            storvsc_exec_srb(
                st.stor_disk.lun,
                &cdb,
                SCSI_IOCTL_DATA_IN,
                chunk_bytes as u32,
                true,
            )?;
            fence(Ordering::Acquire);
            if let Some(ref mut dst) = read_dst {
                core::ptr::copy_nonoverlapping(
                    st.stor_dma_page as *const u8,
                    dst.as_mut_ptr().add(byte_off),
                    chunk_bytes,
                );
            }
        }

        cur_lba += chunk as u64;
        rem -= chunk;
        byte_off += chunk_bytes;
    }

    Ok(())
}

/// Read `count` 512-byte sectors starting at `lba` from the Hyper-V StorVSC SCSI disk.
pub fn storvsc_read_sectors(lba: u64, count: u16, out_buf: &mut [u8]) -> Result<(), &'static str> {
    unsafe { storvsc_transfer(lba, count, false, Some(out_buf), None) }
}

/// Write `count` 512-byte sectors starting at `lba` to the Hyper-V StorVSC SCSI disk.
pub fn storvsc_write_sectors(lba: u64, count: u16, in_buf: &[u8]) -> Result<(), &'static str> {
    unsafe { storvsc_transfer(lba, count, true, None, Some(in_buf)) }
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
                let subchannel_index = u16::from_le_bytes([resp[180], resp[181]]);
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
                } else if if_guid == GUID_SCSI_STORVSC
                    && !st.storvsc_ch.offered
                    && subchannel_index == 0
                {
                    st.storvsc_ch.offered = true;
                    st.storvsc_ch.is_pipe = false;
                    st.storvsc_ch.channel_id = channel_id;
                    st.storvsc_ch.connection_id = connection_id;
                }
            }
        }

        crate::kprintln!(
            "[CELEBRON-HV] Discovered {} VMBus offers (Shutdown={}, Heartbeat={}, SynthKbd={}, StorVSC={})",
            st.total_offers,
            st.shutdown_ch.offered,
            st.heartbeat_ch.offered,
            st.keyboard_ch.offered,
            st.storvsc_ch.offered
        );

        // 7. Open Shutdown IC, Heartbeat IC, Synthetic Keyboard & StorVSC SCSI channels
        let mut shutdown_ch = st.shutdown_ch;
        let mut heartbeat_ch = st.heartbeat_ch;
        let mut keyboard_ch = st.keyboard_ch;
        let mut storvsc_ch = st.storvsc_ch;

        open_vmbus_channel(0, &mut shutdown_ch, pmm, vmm, "Shutdown-IC");
        open_vmbus_channel(1, &mut heartbeat_ch, pmm, vmm, "Heartbeat-IC");
        open_vmbus_channel(2, &mut keyboard_ch, pmm, vmm, "Synth-Keyboard");
        open_vmbus_channel(3, &mut storvsc_ch, pmm, vmm, "StorVSC-SCSI");

        st.shutdown_ch = shutdown_ch;
        st.heartbeat_ch = heartbeat_ch;
        st.keyboard_ch = keyboard_ch;
        st.storvsc_ch = storvsc_ch;
        if st.storvsc_ch.open {
            st.stor_dma_page = pmm.alloc_frame().unwrap_or(0);
        }

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

        // 10. Complete StorVSC SCSI initialization handshake & probe LUN 0..4
        if st.storvsc_ch.open && st.stor_dma_page != 0 {
            st.stor_disk = storvsc_handshake_and_probe();
        }

        HyperVStatus {
            vmbus_online: true,
            vmbus_version: st.negotiated_version,
            offers_discovered: st.total_offers,
            shutdown_ic_ready: st.shutdown_ch.negotiated || st.shutdown_ch.open,
            heartbeat_ic_ready: st.heartbeat_ch.negotiated || st.heartbeat_ch.open,
            synth_kbd_ready: st.keyboard_ch.negotiated || st.keyboard_ch.open,
            storvsc_disk: st.stor_disk,
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
