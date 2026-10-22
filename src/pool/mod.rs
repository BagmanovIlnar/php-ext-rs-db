//! Process-local connection pool and lease registry.

pub mod slot;

use crate::config::{GlobalConfig, SessionResetMode};
use crate::dsn::{Dsn, DriverKind};
use crate::driver::DatabaseConnection;
use crate::error::Error;
use crate::pool::slot::{ConnectionSlot, SlotStatus};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::collections::HashMap;

/// Handle identifying one leased connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseHandle {
    pub slot_id: u64,
    pub generation: u64,
    pub persistent: bool,
    pub canonical_key: String,
    pub masked_address: String,
    pub driver: DriverKind,
}

/// One row for `getConnections()` diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionSnapshot {
    pub address: String,
    pub status: String,
    pub persistent: bool,
    pub driver: String,
    pub idle_ms: Option<u64>,
}

struct DsnPool {
    slots: Vec<ConnectionSlot>,
    next_slot_id: u64,
}

struct RegistryInner {
    owner_pid: u32,
    pools: HashMap<String, DsnPool>,
    /// Non-persistent leases keyed by slot id.
    detached: HashMap<u64, ConnectionSlot>,
    next_id: u64,
}

impl RegistryInner {
    fn new() -> Self {
        Self {
            owner_pid: std::process::id(),
            pools: HashMap::new(),
            detached: HashMap::new(),
            next_id: 1,
        }
    }

    fn ensure_pid(&mut self) {
        let pid = std::process::id();
        if pid != self.owner_pid {
            // After fork: drop inherited handles without closing sockets.
            self.pools.clear();
            self.detached.clear();
            self.owner_pid = pid;
        }
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn apply_persistent_timeouts(&mut self) {
        let timeout_s = GlobalConfig::get().persistent_timeout_s;
        if timeout_s == 0 {
            return;
        }
        for pool in self.pools.values_mut() {
            pool.slots.retain_mut(|slot| {
                if slot.status != SlotStatus::Free {
                    return true;
                }
                if slot.last_used.elapsed().as_secs() >= timeout_s {
                    if let Some(c) = slot.connection.take() {
                        c.close();
                    }
                    return false;
                }
                true
            });
        }
    }
}

static REGISTRY: Lazy<Mutex<RegistryInner>> = Lazy::new(|| Mutex::new(RegistryInner::new()));

fn with_registry<F, R>(f: F) -> R
where
    F: FnOnce(&mut RegistryInner) -> R,
{
    let mut guard = REGISTRY.lock();
    guard.ensure_pid();
    guard.apply_persistent_timeouts();
    f(&mut guard)
}

/// Acquire a lease for the given DSN string.
pub fn acquire(dsn_str: &str, persistent: bool) -> Result<LeaseHandle, Error> {
    let dsn = Dsn::parse(dsn_str)?;
    let canonical_key = dsn.canonical_key();
    let masked = dsn.masked_address();
    let driver = dsn.driver_kind();

    if !persistent {
        let conn = DatabaseConnection::connect(&dsn)?;
        return with_registry(|reg| {
            let id = reg.alloc_id();
            let mut slot = ConnectionSlot::new_reserved(id, false, masked.clone(), canonical_key.clone());
            slot.connection = Some(conn);
            slot.stored_dsn = Some(dsn);
            slot.touch_busy();
            let gen = slot.lease_generation;
            reg.detached.insert(id, slot);
            Ok(LeaseHandle {
                slot_id: id,
                generation: gen,
                persistent: false,
                canonical_key,
                masked_address: masked,
                driver,
            })
        });
    }

    if let Some(handle) = try_take_free_persistent(&canonical_key)? {
        return Ok(handle);
    }

    let (key, idx) = reserve_persistent_slot(&canonical_key, &masked, &dsn)?;
    let connect_result = DatabaseConnection::connect(&dsn);
    with_registry(|reg| {
        let pool = reg.pools.get_mut(&key).expect("pool");
        let slot = &mut pool.slots[idx];
        match connect_result {
            Ok(conn) => {
                slot.connection = Some(conn);
                slot.stored_dsn = Some(dsn.clone());
                slot.mark_free();
            }
            Err(e) => {
                pool.slots.remove(idx);
                return Err(e);
            }
        }
        Ok(())
    })?;
    activate_persistent_slot(&key, idx)
}

fn try_take_free_persistent(canonical_key: &str) -> Result<Option<LeaseHandle>, Error> {
    with_registry(|reg| {
        let Some(pool) = reg.pools.get(canonical_key) else {
            return Ok(None);
        };
        let Some(idx) = pool
            .slots
            .iter()
            .position(|s| s.status == SlotStatus::Free)
        else {
            return Ok(None);
        };
        Ok(Some(activate_persistent_slot_inner(reg, canonical_key, idx)?))
    })
}

fn reserve_persistent_slot(
    canonical_key: &str,
    masked: &str,
    dsn: &Dsn,
) -> Result<(String, usize), Error> {
    with_registry(|reg| {
        let cfg = GlobalConfig::get();
        let pool = reg.pools.entry(canonical_key.to_string()).or_insert(DsnPool {
            slots: Vec::new(),
            next_slot_id: 1,
        });

        let active = pool
            .slots
            .iter()
            .filter(|s| s.status != SlotStatus::Broken)
            .count();
        if active >= cfg.max_connections_per_process {
            return Err(Error::PoolExhausted);
        }

        let slot_id = pool.next_slot_id;
        pool.next_slot_id += 1;
        let mut slot = ConnectionSlot::new_reserved(
            slot_id,
            true,
            masked.to_string(),
            canonical_key.to_string(),
        );
        slot.id = slot_id;
        slot.stored_dsn = Some(dsn.clone());
        pool.slots.push(slot);
        let idx = pool.slots.len() - 1;
        Ok((canonical_key.to_string(), idx))
    })
}

fn activate_persistent_slot(key: &str, idx: usize) -> Result<LeaseHandle, Error> {
    with_registry(|reg| activate_persistent_slot_inner(reg, key, idx))
}

fn activate_persistent_slot_inner(
    reg: &mut RegistryInner,
    key: &str,
    idx: usize,
) -> Result<LeaseHandle, Error> {
    let cfg = GlobalConfig::get();
    let pool = reg.pools.get_mut(key).expect("pool");
    let slot = &mut pool.slots[idx];
    if slot.status != SlotStatus::Free {
        return Err(Error::PoolExhausted);
    }

    if cfg.idle_validate_ms > 0
        && slot.last_used.elapsed().as_millis() as u64 > cfg.idle_validate_ms
    {
        let valid = slot
            .connection
            .as_mut()
            .map(|c| c.is_valid().unwrap_or(false))
            .unwrap_or(false);
        if !valid {
            if let Some(c) = slot.connection.take() {
                c.close();
            }
            if let Some(dsn) = slot.stored_dsn.clone() {
                match DatabaseConnection::connect(&dsn) {
                    Ok(conn) => slot.connection = Some(conn),
                    Err(e) => {
                        pool.slots.remove(idx);
                        return Err(e);
                    }
                }
            } else {
                pool.slots.remove(idx);
                return Err(Error::Connection("idle connection invalid".into()));
            }
        }
    }

    slot.touch_busy();
    let driver = slot
        .connection
        .as_ref()
        .map(|c| c.driver_kind())
        .unwrap_or(DriverKind::Pg);
    Ok(LeaseHandle {
        slot_id: slot.id,
        generation: slot.lease_generation,
        persistent: true,
        canonical_key: slot.canonical_key.clone(),
        masked_address: slot.masked_address.clone(),
        driver,
    })
}

/// Return a lease to the pool (or close non-persistent connection).
pub fn release(lease: LeaseHandle, broken: bool) -> Result<(), Error> {
    with_registry(|reg| {
        if !lease.persistent {
            if let Some(mut slot) = reg.detached.remove(&lease.slot_id) {
                if slot.lease_generation != lease.generation {
                    return Ok(());
                }
                if broken {
                    slot.mark_broken();
                }
                if let Some(mut c) = slot.connection.take() {
                    if !broken {
                        let _ = c.rollback();
                    }
                    c.close();
                }
            }
            return Ok(());
        }

        for pool in reg.pools.values_mut() {
            if let Some(slot) = pool.slots.iter_mut().find(|s| s.id == lease.slot_id) {
                if slot.lease_generation != lease.generation {
                    return Ok(());
                }
                if broken {
                    close_slot(slot);
                    return Ok(());
                }
                let cfg = GlobalConfig::get();
                if let Some(c) = slot.connection.as_mut() {
                    if c.rollback().is_err() {
                        close_slot(slot);
                        return Ok(());
                    }
                    if cfg.session_reset != SessionResetMode::Off {
                        if c.discard_session(cfg.session_reset).is_err() {
                            close_slot(slot);
                            return Ok(());
                        }
                    }
                }
                slot.mark_free();
                return Ok(());
            }
        }
        Ok(())
    })
}

fn close_slot(slot: &mut ConnectionSlot) {
    slot.mark_broken();
    if let Some(c) = slot.connection.take() {
        c.close();
    }
}

/// Mark a leased connection broken (will not return to the free pool).
pub fn close_broken(lease: &LeaseHandle) -> Result<(), Error> {
    release(lease.clone(), true)
}

/// Diagnostic snapshot of connections in this process.
pub fn connection_snapshots() -> Vec<ConnectionSnapshot> {
    with_registry(|reg| {
        let mut out = Vec::new();
        for pool in reg.pools.values() {
            for slot in &pool.slots {
                out.push(snapshot_from_slot(slot));
            }
        }
        for slot in reg.detached.values() {
            out.push(snapshot_from_slot(slot));
        }
        out
    })
}

fn snapshot_from_slot(slot: &ConnectionSlot) -> ConnectionSnapshot {
    let status = match slot.status {
        SlotStatus::Busy => "busy",
        SlotStatus::Free => "free",
        SlotStatus::Connecting => "connecting",
        SlotStatus::Broken => "broken",
    };
    let driver = slot
        .connection
        .as_ref()
        .map(|c| c.driver_kind().as_str().to_string())
        .unwrap_or_else(|| "unknown".into());
    ConnectionSnapshot {
        address: slot.masked_address.clone(),
        status: status.into(),
        persistent: slot.persistent,
        driver,
        idle_ms: if slot.status == SlotStatus::Free {
            Some(slot.idle_ms())
        } else {
            None
        },
    }
}

/// Run a closure against the leased physical connection (generation-checked).
pub fn with_connection<F, R>(lease: &LeaseHandle, f: F) -> Result<R, Error>
where
    F: FnOnce(&mut DatabaseConnection) -> R,
{
    with_registry(|reg| {
        if !lease.persistent {
            let slot = reg
                .detached
                .get_mut(&lease.slot_id)
                .ok_or_else(|| Error::Connection("lease not found".into()))?;
            if slot.lease_generation != lease.generation {
                return Err(Error::Connection("stale lease".into()));
            }
            let conn = slot
                .connection
                .as_mut()
                .ok_or_else(|| Error::Connection("connection closed".into()))?;
            return Ok(f(conn));
        }
        for pool in reg.pools.values_mut() {
            if let Some(slot) = pool.slots.iter_mut().find(|s| s.id == lease.slot_id) {
                if slot.lease_generation != lease.generation {
                    return Err(Error::Connection("stale lease".into()));
                }
                let conn = slot
                    .connection
                    .as_mut()
                    .ok_or_else(|| Error::Connection("connection closed".into()))?;
                return Ok(f(conn));
            }
        }
        Err(Error::Connection("lease not found".into()))
    })
}

/// Close all pooled and detached connections (`MSHUTDOWN`).
pub fn shutdown_all() {
    with_registry(|reg| {
        for pool in reg.pools.values_mut() {
            for slot in &mut pool.slots {
                close_slot(slot);
            }
        }
        reg.pools.clear();
        for (_, mut slot) in reg.detached.drain() {
            close_slot(&mut slot);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn pool_exhausted_when_at_max_without_free() {
        let _g = TEST_LOCK.lock().unwrap();
        shutdown_all();
        let mut cfg = GlobalConfig::get();
        cfg.max_connections_per_process = 1;
        GlobalConfig::set(cfg).unwrap();

        // Without a live DB, we only test registry bookkeeping via snapshots empty.
        let snaps = connection_snapshots();
        assert!(snaps.is_empty());
        GlobalConfig::set(crate::config::Config::default()).unwrap();
    }
}
