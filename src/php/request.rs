//! Per-request registry (`RINIT` → `RSHUTDOWN`).
//!
//! The registry owns three things, all request-local (thread-local, PHP thread only):
//!
//! 1. `by_key`: canonical DSN → strong reference to the `RsDataBaseConnect` object
//!    handed out by `getInstance()` (so `$a === $b` for equal addresses).
//! 2. `states`: weak handles to **every** connection state created in this request
//!    (including `getNewInstance()` objects, the "orphans" that the registry does not
//!    keep alive) so `RSHUTDOWN` can release their leases.
//! 3. `binds`: weak handles to every bind so `RSHUTDOWN` can drop the PHP references
//!    they hold.
//!
//! # Example
//!
//! ```ignore
//! // RINIT
//! request::reset();
//! // ... script runs, getInstance() inserts / looks up entries ...
//! // RSHUTDOWN
//! request::shutdown(); // release leases, invalidate binds, drop object references
//! ```

use crate::php::bind::BindState;
use crate::php::connect::{close_state, ConnState};
use ext_php_rs::types::Zval;
use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::{Rc, Weak};

/// Shared handle to connection state.
pub type StateRef = Rc<RefCell<ConnState>>;

struct Entry {
    zval: Zval,
    state: StateRef,
}

#[derive(Default)]
struct RequestRegistry {
    by_key: HashMap<String, Entry>,
    states: Vec<Weak<RefCell<ConnState>>>,
    binds: Vec<Weak<RefCell<BindState>>>,
}

thread_local! {
    static REGISTRY: RefCell<RequestRegistry> = RefCell::new(RequestRegistry::default());
}

/// Prune dead weak entries once the list grows past this size.
const PRUNE_THRESHOLD: usize = 256;

/// Look up the object registered for a canonical DSN.
///
/// Returns a **new owning zval** (refcount incremented) plus the shared state.
///
/// # Example
///
/// ```ignore
/// if let Some((zval, state)) = request::lookup(&dsn.canonical_key()) { return Ok(zval); }
/// ```
pub fn lookup(key: &str) -> Option<(Zval, StateRef)> {
    REGISTRY.with(|r| {
        r.borrow()
            .by_key
            .get(key)
            .map(|e| (e.zval.shallow_clone(), e.state.clone()))
    })
}

/// Register `zval` (an owning reference) for `key`. A previous entry is returned
/// so the caller can drop it **outside** of the registry borrow.
///
/// # Example
///
/// ```ignore
/// let old = request::insert(key, zval.shallow_clone(), state.clone());
/// drop(old);
/// ```
pub fn insert(key: String, zval: Zval, state: StateRef) -> Option<Zval> {
    REGISTRY.with(|r| {
        r.borrow_mut()
            .by_key
            .insert(key, Entry { zval, state })
            .map(|e| e.zval)
    })
}

/// Remove the entry for `key` only if it belongs to `state`.
///
/// The returned zval must be dropped by the caller after all `RefCell` borrows
/// are released (dropping may free the PHP object).
///
/// # Example
///
/// ```ignore
/// let z = request::remove_if(&key, &state);
/// drop(z);
/// ```
pub fn remove_if(key: &str, state: &StateRef) -> Option<Zval> {
    REGISTRY.with(|r| {
        let mut r = r.borrow_mut();
        let same = r
            .by_key
            .get(key)
            .map(|e| Rc::ptr_eq(&e.state, state))
            .unwrap_or(false);
        if same {
            r.by_key.remove(key).map(|e| e.zval)
        } else {
            None
        }
    })
}

/// Track a connection state so `RSHUTDOWN` can release its lease.
///
/// # Example
///
/// ```ignore
/// request::track_state(&state);
/// ```
pub fn track_state(state: &StateRef) {
    REGISTRY.with(|r| {
        let mut r = r.borrow_mut();
        if r.states.len() >= PRUNE_THRESHOLD {
            r.states.retain(|w| w.strong_count() > 0);
        }
        r.states.push(Rc::downgrade(state));
    });
}

/// Track a bind so `RSHUTDOWN` can invalidate it.
///
/// # Example
///
/// ```ignore
/// request::track_bind(&bind_state);
/// ```
pub fn track_bind(bind: &Rc<RefCell<BindState>>) {
    REGISTRY.with(|r| {
        let mut r = r.borrow_mut();
        if r.binds.len() >= PRUNE_THRESHOLD {
            r.binds.retain(|w| w.strong_count() > 0);
        }
        r.binds.push(Rc::downgrade(bind));
    });
}

/// Number of `getInstance()` objects currently registered (diagnostics/tests).
///
/// # Example
///
/// ```ignore
/// assert_eq!(request::registered_count(), 0);
/// ```
pub fn registered_count() -> usize {
    REGISTRY.with(|r| r.borrow().by_key.len())
}

/// `RINIT`: start every request with an empty registry.
///
/// Anything left over (should not happen) is released first.
///
/// # Example
///
/// ```ignore
/// request::reset();
/// ```
pub fn reset() {
    shutdown();
}

/// `RSHUTDOWN`: release all leases (registry + orphans), invalidate binds and
/// drop the references to PHP objects.
///
/// Runs even after `exit` / fatal errors; each step is isolated with `catch_unwind`
/// so one failing lease cannot leak the others. Closing is idempotent, so objects
/// destroyed after this point do nothing.
///
/// # Example
///
/// ```ignore
/// request::shutdown();
/// ```
pub fn shutdown() {
    let (entries, states, binds) = REGISTRY.with(|r| {
        let mut r = r.borrow_mut();
        (
            std::mem::take(&mut r.by_key),
            std::mem::take(&mut r.states),
            std::mem::take(&mut r.binds),
        )
    });

    for w in binds {
        if let Some(b) = w.upgrade() {
            let _ = catch_unwind(AssertUnwindSafe(|| b.borrow_mut().invalidate()));
        }
    }
    for w in states {
        if let Some(s) = w.upgrade() {
            let _ = catch_unwind(AssertUnwindSafe(|| close_state(&s)));
        }
    }
    for (_, entry) in entries.iter() {
        let st = entry.state.clone();
        let _ = catch_unwind(AssertUnwindSafe(|| close_state(&st)));
    }
    // Drop PHP object references last, with no registry borrow held.
    drop(entries);
}
