//! Single connection slot in a persistent pool.

use crate::driver::DatabaseConnection;
use crate::dsn::Dsn;
use std::time::Instant;

/// Lifecycle state of a pool slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotStatus {
    Connecting,
    Busy,
    Free,
    Broken,
}

/// One physical connection entry in a DSN pool (or a detached non-persistent lease).
pub struct ConnectionSlot {
    pub id: u64,
    pub status: SlotStatus,
    pub persistent: bool,
    pub last_used: Instant,
    pub created_at: Instant,
    pub lease_generation: u64,
    pub connection: Option<DatabaseConnection>,
    pub masked_address: String,
    pub canonical_key: String,
    pub owner_pid: u32,
    /// Parsed DSN (includes credentials) for idle reconnect; not exposed to PHP.
    pub stored_dsn: Option<Dsn>,
}

impl ConnectionSlot {
    pub fn new_reserved(
        id: u64,
        persistent: bool,
        masked_address: String,
        canonical_key: String,
    ) -> Self {
        let now = Instant::now();
        Self {
            id,
            status: SlotStatus::Connecting,
            persistent,
            last_used: now,
            created_at: now,
            lease_generation: 0,
            connection: None,
            masked_address,
            canonical_key,
            owner_pid: std::process::id(),
            stored_dsn: None,
        }
    }

    pub fn touch_busy(&mut self) {
        self.status = SlotStatus::Busy;
        self.last_used = Instant::now();
        self.lease_generation = self.lease_generation.wrapping_add(1);
    }

    pub fn mark_free(&mut self) {
        self.status = SlotStatus::Free;
        self.last_used = Instant::now();
    }

    pub fn mark_broken(&mut self) {
        self.status = SlotStatus::Broken;
    }

    pub fn idle_ms(&self) -> u64 {
        self.last_used.elapsed().as_millis() as u64
    }
}
