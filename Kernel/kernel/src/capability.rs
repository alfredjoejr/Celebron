//! Celebron OS Capability-Based Security System.
//! Enforces unforgeable object capabilities in Ring-0 kernel space.
//! Every privileged kernel resource (system console, hardware timers, memory regions,
//! IPC endpoints, thread controls) is gated by a capability handle held by userspace tasks.

use core::sync::atomic::{AtomicU64, Ordering};

pub const MAX_CAPABILITIES: usize = 64;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CapType {
    Empty = 0,
    System = 1,
    Memory = 2,
    IpcEndpoint = 3,
    Thread = 4,
    DeviceMmio = 5,
    DmaBuffer = 6,
    Irq = 7,
}

#[allow(dead_code)]
pub struct CapRights;
#[allow(dead_code)]
impl CapRights {
    pub const NONE: u32 = 0;
    pub const READ: u32 = 1 << 0;
    pub const WRITE: u32 = 1 << 1;
    pub const EXECUTE: u32 = 1 << 2;
    pub const GRANT: u32 = 1 << 3;
    pub const REVOKE: u32 = 1 << 4;
    pub const SEND: u32 = 1 << 5;
    pub const RECV: u32 = 1 << 6;
    pub const CALL: u32 = (1 << 5) | (1 << 6);
    pub const ALL: u32 = 0x7F;
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapError {
    InvalidHandle,
    PermissionDenied,
    TableFull,
    InvalidOperation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capability {
    pub cap_type: CapType,
    pub rights: u32,
    pub object_id: u64,
    pub badge: u64,
}

impl Capability {
    pub const fn empty() -> Self {
        Self {
            cap_type: CapType::Empty,
            rights: 0,
            object_id: 0,
            badge: 0,
        }
    }

    pub const fn new(cap_type: CapType, rights: u32, object_id: u64, badge: u64) -> Self {
        Self {
            cap_type,
            rights,
            object_id,
            badge,
        }
    }

    pub fn has_rights(&self, required: u32) -> bool {
        (self.rights & required) == required
    }
}

pub struct CapabilityTable {
    slots: [Capability; MAX_CAPABILITIES],
    active_count: usize,
}

impl CapabilityTable {
    pub const fn new() -> Self {
        Self {
            slots: [const { Capability::empty() }; MAX_CAPABILITIES],
            active_count: 0,
        }
    }

    /// Insert a capability into the specified slot (used for standard initial handles).
    pub fn insert_at(&mut self, slot: usize, cap: Capability) -> Result<u32, CapError> {
        if slot >= MAX_CAPABILITIES {
            return Err(CapError::InvalidHandle);
        }
        if self.slots[slot].cap_type == CapType::Empty {
            self.active_count += 1;
        }
        self.slots[slot] = cap;
        Ok(slot as u32)
    }

    /// Allocate the first free slot and insert a capability.
    pub fn allocate(&mut self, cap: Capability) -> Result<u32, CapError> {
        for idx in 1..MAX_CAPABILITIES {
            if self.slots[idx].cap_type == CapType::Empty {
                self.slots[idx] = cap;
                self.active_count += 1;
                return Ok(idx as u32);
            }
        }
        Err(CapError::TableFull)
    }

    /// Look up a capability by handle without permission checks.
    pub fn get(&self, handle: u32) -> Result<&Capability, CapError> {
        let idx = handle as usize;
        if idx == 0 || idx >= MAX_CAPABILITIES {
            return Err(CapError::InvalidHandle);
        }
        let cap = &self.slots[idx];
        if cap.cap_type == CapType::Empty {
            return Err(CapError::InvalidHandle);
        }
        Ok(cap)
    }

    /// Verify that `handle` exists and grants `required_rights`.
    pub fn verify(&self, handle: u32, required_rights: u32) -> Result<&Capability, CapError> {
        let cap = self.get(handle)?;
        if !cap.has_rights(required_rights) {
            return Err(CapError::PermissionDenied);
        }
        Ok(cap)
    }

    /// Derive a child capability with a subset of permissions.
    pub fn derive(&mut self, handle: u32, restricted_rights: u32) -> Result<u32, CapError> {
        let parent = *self.verify(handle, CapRights::GRANT)?;
        if (restricted_rights & !parent.rights) != 0 {
            return Err(CapError::PermissionDenied);
        }
        let child = Capability {
            cap_type: parent.cap_type,
            rights: restricted_rights,
            object_id: parent.object_id,
            badge: parent.badge,
        };
        self.allocate(child)
    }

    /// Revoke and clear a capability slot.
    pub fn revoke(&mut self, handle: u32) -> Result<(), CapError> {
        let idx = handle as usize;
        if idx == 0 || idx >= MAX_CAPABILITIES {
            return Err(CapError::InvalidHandle);
        }
        if self.slots[idx].cap_type == CapType::Empty {
            return Err(CapError::InvalidHandle);
        }
        self.slots[idx] = Capability::empty();
        self.active_count = self.active_count.saturating_sub(1);
        Ok(())
    }

    #[allow(dead_code)]
    pub fn active_count(&self) -> usize {
        self.active_count
    }
}

// Well-known standard initial capability handles for Ring-3 tasks:
pub const CAP_SYS_CONSOLE: u32 = 1;
pub const CAP_SYS_INFO: u32 = 2;
pub const CAP_IPC_ENDPOINT_INIT: u32 = 3;
pub const CAP_MEM_SHARED: u32 = 4;
pub const CAP_SYS_POWER: u32 = 5;
pub const CAP_DRV_MMIO: u32 = 6;
pub const CAP_DRV_DMA: u32 = 7;
pub const CAP_DRV_IRQ: u32 = 8;

#[allow(dead_code)]
static CAP_OPERATIONS_COUNT: AtomicU64 = AtomicU64::new(0);

#[allow(dead_code)]
pub fn record_cap_op() {
    CAP_OPERATIONS_COUNT.fetch_add(1, Ordering::Relaxed);
}

#[allow(dead_code)]
pub fn cap_operations_count() -> u64 {
    CAP_OPERATIONS_COUNT.load(Ordering::Relaxed)
}

/// Run an in-kernel self-test verifying capability insertion, right verification,
/// derivation diminution, and unauthorized access rejection.
pub fn run_self_test() -> bool {
    let mut table = CapabilityTable::new();

    // 1. Insert Console capability with WRITE rights
    let console_cap = Capability::new(CapType::System, CapRights::WRITE | CapRights::GRANT, 0x1, 0);
    assert!(table.insert_at(CAP_SYS_CONSOLE as usize, console_cap).is_ok());

    // 2. Verify WRITE right succeeds
    assert!(table.verify(CAP_SYS_CONSOLE, CapRights::WRITE).is_ok());

    // 3. Verify READ right fails with PermissionDenied
    assert_eq!(
        table.verify(CAP_SYS_CONSOLE, CapRights::READ),
        Err(CapError::PermissionDenied)
    );

    // 4. Derive child with reduced rights
    let child_handle = table.derive(CAP_SYS_CONSOLE, CapRights::WRITE).expect("derive failed");
    assert!(table.verify(child_handle, CapRights::WRITE).is_ok());
    assert_eq!(
        table.verify(child_handle, CapRights::GRANT),
        Err(CapError::PermissionDenied)
    );

    // 5. Revoke child
    assert!(table.revoke(child_handle).is_ok());
    assert_eq!(table.get(child_handle), Err(CapError::InvalidHandle));

    // 6. Verify non-existent handle rejection
    assert_eq!(table.get(999), Err(CapError::InvalidHandle));

    true
}
