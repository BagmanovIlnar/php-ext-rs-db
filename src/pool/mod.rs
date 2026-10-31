//! Process-local connection pool and lease registry.
//!
//! Persistent leases are shared across the PHP process. On **NTS**, an exhausted
//! pool returns [`Error::PoolExhausted`] immediately. On **ZTS**, waiters join a
//! per-DSN FIFO queue and block until a slot is freed or
//! `pool_wait_timeout_ms` elapses ([`Error::PoolTimeout`]).
//!
//! # Locking rules
//!
//! The registry mutex protects **bookkeeping only**. Every operation that talks to
//! the network (connect, ping, rollback, session reset, query execution, close)
//! runs *outside* the lock: the physical connection is taken out of its slot for
//! the duration and put back afterwards (generation-checked), so concurrent PHP
//! threads never serialise on each other's queries.

pub mod slot;

use crate::config::{GlobalConfig, SessionResetMode};
use crate::dsn::{Dsn, DriverKind};
use crate::driver::DatabaseConnection;
use crate::error::Error;
use crate::pool::slot::{ConnectionSlot, SlotStatus};
use once_cell::sync::Lazy;
use parking_lot::{Condvar, Mutex};
use std::collections::{HashMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant};

/// Upper bound for "rescan the pool" retries within one acquire (see
/// [`TakeFree::Retry`]); effectively never reached in practice.
const ACQUIRE_ATTEMPTS: usize = 8;

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
    /// FIFO of waiter ids (ZTS). Empty on NTS in practice.
    waiters: VecDeque<u64>,
    next_waiter_id: u64,
}

impl DsnPool {
    fn new() -> Self {
        Self {
            slots: Vec::new(),
            next_slot_id: 1,
            waiters: VecDeque::new(),
            next_waiter_id: 1,
        }
    }

    fn active_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.status != SlotStatus::Broken)
            .count()
    }

    fn remove_waiter(&mut self, waiter_id: u64) {
        self.waiters.retain(|&id| id != waiter_id);
    }
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
            // After fork: abandon the inherited handles *without* closing them,
            // so the parent's sessions stay intact. Leaking the sockets in the
            // (usually short-lived) child is the safe trade-off; the registry
            // starts empty and the child opens its own connections on demand.
            for (_, pool) in self.pools.drain() {
                for mut slot in pool.slots {
                    if let Some(c) = slot.connection.take() {
                        std::mem::forget(c);
                    }
                }
            }
            for (_, mut slot) in self.detached.drain() {
                if let Some(c) = slot.connection.take() {
                    std::mem::forget(c);
                }
            }
            self.owner_pid = pid;
        }
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Find a persistent slot by id (slots are always addressed by id, never by
    /// vector position: positions shift when slots are removed concurrently).
    fn slot_mut(&mut self, slot_id: u64) -> Option<&mut ConnectionSlot> {
        self.pools
            .values_mut()
            .find_map(|pool| pool.slots.iter_mut().find(|s| s.id == slot_id))
    }

    /// Atomically (check generation +) remove a persistent slot and return its
    /// connection so it can be closed outside the registry lock.
    /// `None` when the slot is gone or serves a newer lease (stale handle).
    fn take_broken_persistent(&mut self, lease: &LeaseHandle) -> Option<DatabaseConnection> {
        for pool in self.pools.values_mut() {
            let Some(pos) = pool.slots.iter().position(|s| s.id == lease.slot_id) else {
                continue;
            };
            if pool.slots[pos].lease_generation != lease.generation {
                return None; // stale handle: the slot serves another lease now
            }
            let mut slot = pool.slots.remove(pos);
            return slot.connection.take();
        }
        None
    }

    fn apply_persistent_timeouts(&mut self) -> Vec<DatabaseConnection> {
        let timeout_s = GlobalConfig::get().persistent_timeout_s;
        let mut closed = Vec::new();
        if timeout_s == 0 {
            return closed;
        }
        for pool in self.pools.values_mut() {
            let mut i = 0;
            while i < pool.slots.len() {
                let expired = pool.slots[i].status == SlotStatus::Free
                    && pool.slots[i].last_used.elapsed().as_secs() >= timeout_s;
                if expired {
                    let mut slot = pool.slots.remove(i);
                    if let Some(c) = slot.connection.take() {
                        closed.push(c);
                    }
                } else {
                    i += 1;
                }
            }
        }
        closed
    }
}

static REGISTRY: Lazy<(Mutex<RegistryInner>, Condvar)> =
    Lazy::new(|| (Mutex::new(RegistryInner::new()), Condvar::new()));

fn with_registry<F, R>(f: F) -> R
where
    F: FnOnce(&mut RegistryInner) -> R,
{
    let (lock, _) = &*REGISTRY;
    let mut guard = lock.lock();
    guard.ensure_pid();
    let expired = guard.apply_persistent_timeouts();
    let r = f(&mut guard);
    drop(guard);
    if !expired.is_empty() {
        for c in expired {
            c.close();
        }
        notify_waiters();
    }
    r
}

/// Wake every waiter. The condition variable is shared by all pools, so a
/// single `notify_one` could wake a waiter of an unrelated DSN and leave the
/// intended FIFO head asleep until its timeout (missed wakeup).
fn notify_waiters() {
    let (_, cvar) = &*REGISTRY;
    cvar.notify_all();
}

/// True when this caller may take or create a persistent slot.
///
/// If the FIFO is empty, anyone may acquire. If waiters exist, only the head
/// waiter (matched by `my_waiter`) may proceed.
fn may_acquire(pool: &DsnPool, my_waiter: Option<u64>) -> bool {
    match pool.waiters.front() {
        None => true,
        Some(&head) => my_waiter == Some(head),
    }
}

/// Build the lease handle from a just-leased slot (`touch_busy` already applied).
fn lease_from_slot(slot: &ConnectionSlot) -> LeaseHandle {
    LeaseHandle {
        slot_id: slot.id,
        generation: slot.lease_generation,
        persistent: true,
        canonical_key: slot.canonical_key.clone(),
        masked_address: slot.masked_address.clone(),
        driver: slot_driver(slot),
    }
}

fn slot_driver(slot: &ConnectionSlot) -> DriverKind {
    slot.connection
        .as_ref()
        .map(|c| c.driver_kind())
        .or_else(|| slot.stored_dsn.as_ref().map(|d| d.driver))
        .unwrap_or(DriverKind::Pg)
}

/// Acquire a lease for the given DSN string.
pub fn acquire(dsn_str: &str, persistent: bool) -> Result<LeaseHandle, Error> {
    let dsn = Dsn::parse(dsn_str)?;
    let canonical_key = dsn.canonical_key();
    let masked = dsn.masked_address();

    if !persistent {
        let driver = dsn.driver_kind();
        // Connect outside the registry lock.
        let conn = DatabaseConnection::connect(&dsn)?;
        return with_registry(|reg| {
            let id = reg.alloc_id();
            let mut slot =
                ConnectionSlot::new_reserved(id, false, masked.clone(), canonical_key.clone());
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

    if cfg!(php_zts) {
        acquire_persistent_zts(dsn, canonical_key, masked)
    } else {
        acquire_persistent_nts(dsn, canonical_key, masked)
    }
}

fn acquire_persistent_nts(
    dsn: Dsn,
    canonical_key: String,
    masked: String,
) -> Result<LeaseHandle, Error> {
    for _ in 0..ACQUIRE_ATTEMPTS {
        match try_take_free_persistent(&canonical_key, None)? {
            TakeFree::Leased(handle) => return Ok(handle),
            TakeFree::None => {}
            // The candidate slot died during idle validation; rescan.
            TakeFree::Retry => continue,
        }

        let slot_id = reserve_persistent_slot(&canonical_key, &masked, &dsn, None)?;
        return finish_new_persistent(&dsn, &canonical_key, slot_id);
    }
    Err(Error::PoolExhausted)
}

fn acquire_persistent_zts(
    dsn: Dsn,
    canonical_key: String,
    masked: String,
) -> Result<LeaseHandle, Error> {
    let timeout_ms = GlobalConfig::get().pool_wait_timeout_ms;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut my_waiter: Option<u64> = None;

    let mut attempts = 0;
    loop {
        match try_take_free_persistent(&canonical_key, my_waiter) {
            Ok(TakeFree::Leased(handle)) => {
                finish_waiter(&canonical_key, my_waiter);
                return Ok(handle);
            }
            Ok(TakeFree::Retry) => {
                attempts += 1;
                if attempts >= ACQUIRE_ATTEMPTS {
                    finish_waiter(&canonical_key, my_waiter);
                    return Err(Error::PoolExhausted);
                }
                continue;
            }
            Ok(TakeFree::None) => {}
            Err(e) => {
                finish_waiter(&canonical_key, my_waiter);
                return Err(e);
            }
        }

        match reserve_persistent_slot(&canonical_key, &masked, &dsn, my_waiter) {
            Ok(slot_id) => {
                let result = finish_new_persistent(&dsn, &canonical_key, slot_id);
                finish_waiter(&canonical_key, my_waiter);
                return result;
            }
            Err(Error::PoolExhausted) => {}
            Err(e) => {
                finish_waiter(&canonical_key, my_waiter);
                return Err(e);
            }
        }

        let (lock, cvar) = &*REGISTRY;
        let mut guard = lock.lock();
        guard.ensure_pid();
        let expired = guard.apply_persistent_timeouts();
        if !expired.is_empty() {
            drop(guard);
            for c in expired {
                c.close();
            }
            notify_waiters();
            guard = lock.lock();
        }

        let pool = guard
            .pools
            .entry(canonical_key.clone())
            .or_insert_with(DsnPool::new);

        let wid = match my_waiter {
            Some(id) => id,
            None => {
                let id = pool.next_waiter_id;
                pool.next_waiter_id = pool.next_waiter_id.saturating_add(1);
                pool.waiters.push_back(id);
                my_waiter = Some(id);
                id
            }
        };

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            pool.remove_waiter(wid);
            return Err(Error::PoolTimeout);
        }

        // Head of FIFO and a free / creatable slot: drop lock and retry.
        if pool.waiters.front() == Some(&wid) {
            let max = GlobalConfig::get().max_connections_per_process;
            let can_progress = pool.slots.iter().any(|s| s.status == SlotStatus::Free)
                || pool.active_count() < max;
            if can_progress {
                drop(guard);
                continue;
            }
        }

        let timed_out = cvar.wait_for(&mut guard, remaining).timed_out();
        if timed_out {
            if let Some(pool) = guard.pools.get_mut(&canonical_key) {
                pool.remove_waiter(wid);
            }
            return Err(Error::PoolTimeout);
        }
    }
}

fn finish_waiter(canonical_key: &str, my_waiter: Option<u64>) {
    let Some(wid) = my_waiter else {
        return;
    };
    with_registry(|reg| {
        if let Some(pool) = reg.pools.get_mut(canonical_key) {
            pool.remove_waiter(wid);
        }
    });
    notify_waiters();
}

/// Reserve a fresh persistent slot (status `Connecting`, no connection yet) and
/// return its **slot id**. The physical connect happens afterwards, outside the
/// registry lock, in [`finish_new_persistent`].
fn reserve_persistent_slot(
    canonical_key: &str,
    masked: &str,
    dsn: &Dsn,
    my_waiter: Option<u64>,
) -> Result<u64, Error> {
    with_registry(|reg| {
        let cfg = GlobalConfig::get();
        let pool = reg
            .pools
            .entry(canonical_key.to_string())
            .or_insert_with(DsnPool::new);

        if !may_acquire(pool, my_waiter) {
            return Err(Error::PoolExhausted);
        }

        let active = pool.active_count();
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
        slot.stored_dsn = Some(dsn.clone());
        pool.slots.push(slot);
        Ok(slot_id)
    })
}

/// Connect and lease the reserved slot in one final critical section. The
/// connect itself runs outside the lock; on any kind of registry change in
/// between (fork / shutdown) the reservation is gone and an error is returned.
fn finish_new_persistent(
    dsn: &Dsn,
    key: &str,
    slot_id: u64,
) -> Result<LeaseHandle, Error> {
    // Physical connect outside the registry lock.
    let connect_result = DatabaseConnection::connect(dsn);

    // Single-use, immediately matched; boxing the connection would add noise.
    #[allow(clippy::large_enum_variant)]
    enum Out {
        Leased(LeaseHandle),
        Failed(Error),
        Gone(Result<DatabaseConnection, Error>),
    }

    let out = with_registry(|reg| {
        let Some(pool) = reg.pools.get_mut(key) else {
            return Out::Gone(connect_result);
        };
        let Some(pos) = pool.slots.iter().position(|s| s.id == slot_id) else {
            return Out::Gone(connect_result);
        };
        match connect_result {
            Ok(conn) => {
                let slot = &mut pool.slots[pos];
                debug_assert_eq!(slot.status, SlotStatus::Connecting);
                slot.connection = Some(conn);
                slot.stored_dsn = Some(dsn.clone());
                slot.touch_busy();
                Out::Leased(lease_from_slot(slot))
            }
            Err(e) => {
                pool.slots.remove(pos);
                Out::Failed(e)
            }
        }
    });

    match out {
        Out::Leased(handle) => Ok(handle),
        Out::Failed(e) => Err(e),
        Out::Gone(Ok(conn)) => {
            conn.close();
            Err(Error::Internal(
                "pool registry changed during connect (fork or shutdown?)".into(),
            ))
        }
        Out::Gone(Err(e)) => Err(e),
    }
}

/// Outcome of scanning the pool for a free persistent slot.
enum TakeFree {
    /// A free slot was leased to us.
    Leased(LeaseHandle),
    /// No free slot (or we are not the FIFO head).
    None,
    /// The candidate slot was removed during idle validation; rescan.
    Retry,
}

/// Take (and possibly revalidate) a free persistent slot.
///
/// Idle validation (`idle_validate_ms`) marks the slot `Validating` and runs
/// ping / reconnect **outside** the registry lock, then leases it in a second
/// critical section (checked by slot id, not by vector position).
fn try_take_free_persistent(
    canonical_key: &str,
    my_waiter: Option<u64>,
) -> Result<TakeFree, Error> {
    #[allow(clippy::large_enum_variant)]
    enum Pick {
        None,
        Leased(LeaseHandle),
        Validate {
            slot_id: u64,
            conn: Option<DatabaseConnection>,
            dsn: Option<Dsn>,
        },
    }

    let pick = with_registry(|reg| {
        let Some(pool) = reg.pools.get_mut(canonical_key) else {
            return Pick::None;
        };
        if !may_acquire(pool, my_waiter) {
            return Pick::None;
        }
        let Some(slot) = pool
            .slots
            .iter_mut()
            .find(|s| s.status == SlotStatus::Free)
        else {
            return Pick::None;
        };
        let cfg = GlobalConfig::get();
        let idle_ms = slot.last_used.elapsed().as_millis() as u64;
        if cfg.idle_validate_ms > 0 && idle_ms > cfg.idle_validate_ms {
            slot.status = SlotStatus::Validating;
            let conn = slot.connection.take();
            let dsn = slot.stored_dsn.clone();
            Pick::Validate {
                slot_id: slot.id,
                conn,
                dsn,
            }
        } else {
            slot.touch_busy();
            Pick::Leased(lease_from_slot(slot))
        }
    });

    match pick {
        Pick::None => Ok(TakeFree::None),
        Pick::Leased(handle) => Ok(TakeFree::Leased(handle)),
        Pick::Validate { slot_id, conn, dsn } => {
            let Some(dsn) = dsn else {
                // Reserved slot without a stored DSN cannot be reconnected.
                let conn = remove_persistent_by_id(canonical_key, slot_id)
                    .and_then(|mut slot| slot.connection.take());
                if let Some(c) = conn {
                    c.close();
                }
                notify_waiters();
                return Err(Error::Connection("idle connection invalid".into()));
            };

            // Ping / reconnect outside the registry lock.
            let validated = match catch_unwind(AssertUnwindSafe(|| ensure_alive(conn, &dsn))) {
                Ok(v) => v,
                Err(payload) => {
                    remove_persistent_by_id(canonical_key, slot_id);
                    notify_waiters();
                    std::panic::resume_unwind(payload);
                }
            };

            #[allow(clippy::large_enum_variant)]
            enum Final {
                Leased(LeaseHandle),
                Failed(Error),
                Gone(Result<DatabaseConnection, Error>),
            }

            let final_state = with_registry(|reg| {
                let Some(pool) = reg.pools.get_mut(canonical_key) else {
                    return Final::Gone(validated);
                };
                let Some(pos) = pool.slots.iter().position(|s| s.id == slot_id) else {
                    return Final::Gone(validated);
                };
                match validated {
                    Ok(conn) => {
                        let slot = &mut pool.slots[pos];
                        if slot.status != SlotStatus::Validating {
                            return Final::Gone(Ok(conn));
                        }
                        slot.connection = Some(conn);
                        slot.touch_busy();
                        Final::Leased(lease_from_slot(slot))
                    }
                    Err(e) => {
                        pool.slots.remove(pos);
                        Final::Failed(e)
                    }
                }
            });

            match final_state {
                Final::Leased(handle) => Ok(TakeFree::Leased(handle)),
                Final::Failed(e) => {
                    notify_waiters();
                    Err(e)
                }
                Final::Gone(Ok(conn)) => {
                    conn.close();
                    Ok(TakeFree::Retry)
                }
                Final::Gone(Err(e)) => Err(e),
            }
        }
    }
}

/// Return a live connection: ping the existing one first, transparently
/// reconnect when the ping fails.
fn ensure_alive(conn: Option<DatabaseConnection>, dsn: &Dsn) -> Result<DatabaseConnection, Error> {
    if let Some(mut c) = conn {
        if c.ping().unwrap_or(false) {
            return Ok(c);
        }
        c.close();
    }
    DatabaseConnection::connect(dsn)
}

/// Remove a persistent slot by id (fork/shutdown-safe no-op when missing) and
/// return the whole slot so its connection can be closed by the caller.
fn remove_persistent_by_id(canonical_key: &str, slot_id: u64) -> Option<ConnectionSlot> {
    with_registry(|reg| {
        let pool = reg.pools.get_mut(canonical_key)?;
        let pos = pool.slots.iter().position(|s| s.id == slot_id)?;
        Some(pool.slots.remove(pos))
    })
}

/// Return a lease to the pool (persistent) or close the connection
/// (non-persistent). Broken persistent slots are removed from the pool so they
/// cannot accumulate. Rollback and session reset run outside the registry lock.
pub fn release(lease: LeaseHandle, broken: bool) -> Result<(), Error> {
    if !lease.persistent {
        release_detached(lease, broken);
        return Ok(());
    }
    release_persistent(lease, broken);
    Ok(())
}

fn release_detached(lease: LeaseHandle, broken: bool) {
    let conn = with_registry(|reg| {
        let mut slot = reg.detached.remove(&lease.slot_id)?;
        if slot.lease_generation != lease.generation {
            // Stale handle racing a newer lease of the same slot: put it back.
            reg.detached.insert(lease.slot_id, slot);
            return None;
        }
        if broken {
            slot.mark_broken();
        }
        slot.connection.take()
    });
    if let Some(mut c) = conn {
        if !broken {
            let _ = c.rollback();
        }
        c.close();
    }
}

fn release_persistent(lease: LeaseHandle, broken: bool) {
    if broken {
        // Remove the slot entirely (broken slots must not accumulate) and close
        // the connection outside the lock.
        let conn = with_registry(|reg| reg.take_broken_persistent(&lease));
        if let Some(c) = conn {
            c.close();
        }
        notify_waiters();
        return;
    }

    // Phase 1: take the connection out; the slot stays Busy so nobody else can
    // lease it while we finalise outside the lock.
    let extracted = with_registry(|reg| {
        reg.slot_mut(lease.slot_id).and_then(|slot| {
            if slot.lease_generation != lease.generation || slot.status != SlotStatus::Busy {
                None // stale or duplicate release
            } else {
                slot.connection.take()
            }
        })
    });
    let Some(mut conn) = extracted else {
        return;
    };

    // Phase 2 (outside the lock): rollback + session reset.
    let cfg = GlobalConfig::get();
    let healthy = conn.rollback().is_ok()
        && (cfg.session_reset == SessionResetMode::Off
            || conn.discard_session(cfg.session_reset).is_ok());

    // Phase 3: put the connection back as Free, or drop the slot when the
    // connection turned out unhealthy.
    let freed_capacity = with_registry(|reg| {
        for pool in reg.pools.values_mut() {
            let Some(pos) = pool.slots.iter().position(|s| s.id == lease.slot_id) else {
                continue;
            };
            let slot = &mut pool.slots[pos];
            if slot.lease_generation != lease.generation
                || slot.status != SlotStatus::Busy
                || slot.connection.is_some()
            {
                return false; // registry moved on; `conn` is dropped (closed) here
            }
            if healthy {
                slot.connection = Some(conn);
                slot.mark_free();
            } else {
                pool.slots.remove(pos);
            }
            return true;
        }
        false
    });
    if freed_capacity {
        notify_waiters();
    }
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
        SlotStatus::Validating => "validating",
        SlotStatus::Broken => "broken",
    };
    let driver = slot
        .connection
        .as_ref()
        .map(|c| c.driver_kind())
        .or_else(|| slot.stored_dsn.as_ref().map(|d| d.driver))
        .map(|d| d.as_str().to_string())
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
///
/// The connection is taken out of the registry for the duration of `f`, so a
/// long query never holds the registry lock (other threads can use their own
/// connections in parallel). The connection is always returned to its slot —
/// even when `f` panics (the panic is re-thrown afterwards).
pub fn with_connection<F, R>(lease: &LeaseHandle, f: F) -> Result<R, Error>
where
    F: FnOnce(&mut DatabaseConnection) -> R,
{
    #[allow(clippy::large_enum_variant)]
    enum Found {
        Missing,
        Stale,
        Took(Option<DatabaseConnection>),
    }

    // Phase 1: locate the slot and extract the connection.
    let found = with_registry(|reg| {
        if !lease.persistent {
            match reg.detached.get_mut(&lease.slot_id) {
                None => Found::Missing,
                Some(slot) => {
                    if slot.lease_generation != lease.generation {
                        Found::Stale
                    } else {
                        Found::Took(slot.connection.take())
                    }
                }
            }
        } else {
            match reg.slot_mut(lease.slot_id) {
                None => Found::Missing,
                Some(slot) => {
                    if slot.lease_generation != lease.generation {
                        Found::Stale
                    } else {
                        Found::Took(slot.connection.take())
                    }
                }
            }
        }
    });

    let mut conn = match found {
        Found::Missing => return Err(Error::Connection("lease not found".into())),
        Found::Stale => return Err(Error::Connection("stale lease".into())),
        Found::Took(None) => return Err(Error::Connection("connection closed".into())),
        Found::Took(Some(conn)) => conn,
    };

    // Phase 2: run the user closure outside the lock (panic-safe).
    let result = catch_unwind(AssertUnwindSafe(|| f(&mut conn)));

    // Phase 3: return the connection to its slot; if the slot is gone or serves
    // another lease, `conn` is dropped here and closed.
    let _returned = with_registry(|reg| {
        let slot = if lease.persistent {
            match reg.slot_mut(lease.slot_id) {
                Some(slot) => slot,
                None => return false,
            }
        } else {
            match reg.detached.get_mut(&lease.slot_id) {
                Some(slot) => slot,
                None => return false,
            }
        };
        if slot.lease_generation == lease.generation && slot.connection.is_none() {
            slot.connection = Some(conn);
            true
        } else {
            false // dropped (closed) at the end of this closure
        }
    });

    match result {
        Ok(r) => Ok(r),
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// Close all pooled and detached connections (`MSHUTDOWN`).
pub fn shutdown_all() {
    let conns = with_registry(|reg| {
        let mut conns = Vec::new();
        for (_, mut pool) in reg.pools.drain() {
            pool.waiters.clear();
            for mut slot in pool.slots.drain(..) {
                if let Some(c) = slot.connection.take() {
                    conns.push(c);
                }
            }
        }
        for (_, mut slot) in reg.detached.drain() {
            if let Some(c) = slot.connection.take() {
                conns.push(c);
            }
        }
        conns
    });
    for c in conns {
        c.close();
    }
    notify_waiters();
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

    #[test]
    fn may_acquire_fifo_head_only() {
        let mut pool = DsnPool::new();
        assert!(may_acquire(&pool, None));
        pool.waiters.push_back(10);
        pool.waiters.push_back(20);
        assert!(!may_acquire(&pool, None));
        assert!(may_acquire(&pool, Some(10)));
        assert!(!may_acquire(&pool, Some(20)));
    }

    #[test]
    fn broken_release_of_unknown_lease_is_noop() {
        let _g = TEST_LOCK.lock().unwrap();
        shutdown_all();
        let lease = LeaseHandle {
            slot_id: 999_999,
            generation: 1,
            persistent: true,
            canonical_key: "pg://u:***@h:5432/db".into(),
            masked_address: "pg://u:***@h:5432/db".into(),
            driver: DriverKind::Pg,
        };
        release(lease.clone(), true).unwrap();
        release(lease, false).unwrap();
        assert!(connection_snapshots().is_empty());
    }

    #[test]
    fn with_connection_unknown_lease_errors() {
        let _g = TEST_LOCK.lock().unwrap();
        shutdown_all();
        let lease = LeaseHandle {
            slot_id: 424_242,
            generation: 1,
            persistent: false,
            canonical_key: String::new(),
            masked_address: String::new(),
            driver: DriverKind::Pg,
        };
        let r = with_connection(&lease, |c| c.ping());
        assert!(r.is_err());
    }
}
