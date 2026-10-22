//! `RsDataBaseConnect` - connection factory, lease holder and transaction manager.
//!
//! * `getInstance($address, $persistent = false)` returns the **same PHP object**
//!   for the same canonical DSN within one PHP request (request registry).
//! * `getNewInstance()` always takes a new lease; the object is *not* in the registry
//!   but is tracked so `RSHUTDOWN` releases it (and `Drop` equals `close()`).
//! * Transactions: `transaction_level`, savepoints `RSDB_{slot}_{level}` for nested
//!   levels, `tx_failed` handling (PostgreSQL).
//!
//! Shared mutable data lives in [`ConnState`] (`Rc<RefCell<..>>`) so that
//! `RsDataBasePrepare` / `RsDataBaseCursor` can validate the lease and update
//! transaction flags without borrowing the PHP object.
//!
//! # Example (PHP)
//!
//! ```php
//! $a = RsDataBaseConnect::getInstance('pg://u:p@host:5432/db', true);
//! $b = RsDataBaseConnect::getInstance('pg://u:p@host:5432/db', true);
//! var_dump($a === $b);        // true
//! $a->beginTransaction();     // level 1
//! $b->beginTransaction();     // level 2 -> SAVEPOINT RSDB_<slot>_2
//! $b->rollBack();             // ROLLBACK TO SAVEPOINT
//! $a->commit();               // COMMIT
//! ```

use crate::driver::DatabaseConnection;
use crate::dsn::{ColumnCase, DriverKind, Dsn};
use crate::error::Error;
use crate::php::convert::{
    assoc_zval, bool_zval, guard, list_zval, long_zval, str_zval, PhpResult,
};
use crate::php::exception::{connection_error, error_message, error_parts, usage_error};
use crate::php::prepare::RsDataBasePrepare;
use crate::php::request::{self, StateRef};
use crate::pool::{self, LeaseHandle};
use crate::sql;
use ext_php_rs::convert::IntoZval;
use ext_php_rs::ffi;
use ext_php_rs::flags::{ClassFlags, ErrorType};
use ext_php_rs::prelude::*;
use ext_php_rs::types::{ZendClassObject, Zval};
use std::cell::RefCell;
use std::ffi::CString;
use std::os::raw::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;

/// Shared state of one connection lease (PHP object, statements and cursors all see it).
pub struct ConnState {
    /// Current lease; `None` after `close()`.
    pub lease: Option<LeaseHandle>,
    /// Original DSN string, kept **only** for `execute(reconnect: true)`. Never exposed.
    pub dsn_raw: String,
    /// DSN with the password masked (`getAddress()`).
    pub address: String,
    pub driver: DriverKind,
    pub persistent: bool,
    pub canonical_key: String,
    pub column_case: ColumnCase,
    /// DSN `statement_rollback` (PostgreSQL): when off, an error aborts the transaction.
    pub statement_rollback: bool,
    /// Explicit transaction nesting level (`beginTransaction()` calls).
    pub tx_level: u32,
    /// A physical DB transaction is open (explicit or implicit after a statement).
    pub physical_tx_active: bool,
    /// PostgreSQL transaction is in the aborted state (`25P02`) until `rollBack()`.
    pub tx_failed: bool,
    pub closed: bool,
    /// Connection lost: every operation throws until `rollBack()` / `close()`.
    pub broken: bool,
    /// Incremented whenever the lease is replaced or released; children store it.
    pub epoch: u64,
    /// This state is the `getInstance()` object of `canonical_key`.
    pub in_registry: bool,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

impl ConnState {
    /// Create state for a freshly acquired lease.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let st = ConnState::new(lease, &dsn, "pg://u:p@h/db", true);
    /// ```
    pub fn new(lease: LeaseHandle, dsn: &Dsn, dsn_raw: &str, persistent: bool) -> Self {
        Self {
            address: lease.masked_address.clone(),
            driver: lease.driver,
            canonical_key: lease.canonical_key.clone(),
            lease: Some(lease),
            dsn_raw: dsn_raw.to_string(),
            persistent,
            column_case: dsn.column_case,
            statement_rollback: dsn.statement_rollback,
            tx_level: 0,
            physical_tx_active: false,
            tx_failed: false,
            closed: false,
            broken: false,
            epoch: 1,
            in_registry: false,
            error_code: None,
            error_message: None,
        }
    }

    /// Fail with `Error::Connection` if the connection is closed or broken.
    ///
    /// # Example
    ///
    /// ```ignore
    /// state.borrow().ensure_usable()?;
    /// ```
    pub fn ensure_usable(&self) -> Result<(), Error> {
        if self.closed || self.lease.is_none() {
            return Err(Error::Connection("connection is closed".into()));
        }
        if self.broken {
            return Err(Error::Connection(
                "connection is broken; call rollBack() or close()".into(),
            ));
        }
        Ok(())
    }

    /// Remember the last error (code + message) for `getErrorCode()` / `getErrorMessage()`.
    pub fn set_error(&mut self, e: &Error) {
        let (code, msg) = error_parts(e);
        self.error_code = code;
        self.error_message = Some(msg);
    }

    /// Reset the last error.
    pub fn clear_error(&mut self) {
        self.error_code = None;
        self.error_message = None;
    }

    /// Savepoint name for a nesting level: `RSDB_{slot}_{level}`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// assert_eq!(state.savepoint_name(2), "RSDB_7_2");
    /// ```
    pub fn savepoint_name(&self, level: u32) -> String {
        let slot = self.lease.as_ref().map(|l| l.slot_id).unwrap_or(0);
        format!("RSDB_{}_{}", slot, level)
    }
}

/// Heuristic: did the error come from a lost connection (socket closed, reset, ...)?
///
/// # Example
///
/// ```ignore
/// assert!(is_connection_lost(&Error::Connection("closed".into())));
/// assert!(!is_connection_lost(&Error::Driver("[42P01] no table".into())));
/// ```
pub fn is_connection_lost(e: &Error) -> bool {
    match e {
        Error::Connection(_) => true,
        Error::Driver(m) => {
            if m.starts_with('[') {
                return false; // has a SQLSTATE: the server answered
            }
            let l = m.to_ascii_lowercase();
            l.contains("connection closed")
                || l.contains("connection reset")
                || l.contains("broken pipe")
                || l.contains("error communicating with the server")
                || l.contains("unexpected eof")
        }
        _ => false,
    }
}

/// Record a failure on the state: stores the error, marks the connection broken when
/// it was lost and returns the error (as `Error::Connection` when lost).
///
/// # Example
///
/// ```ignore
/// run_on_connection(&state, |c| c.begin()).map_err(|e| note_failure(&state, e))?;
/// ```
pub fn note_failure(state: &StateRef, e: Error) -> Error {
    let lost = is_connection_lost(&e);
    let mut s = state.borrow_mut();
    s.set_error(&e);
    if lost {
        s.broken = true;
        return match e {
            Error::Connection(_) => e,
            other => Error::Connection(error_message(&other)),
        };
    }
    e
}

/// Run `f` on the physical connection of the state's lease (generation-checked).
///
/// The state borrow is released before the call, so `f` may take as long as it needs.
///
/// # Example
///
/// ```ignore
/// let ok = run_on_connection(&state, |c| c.ping())?;
/// ```
pub fn run_on_connection<R>(
    state: &StateRef,
    f: impl FnOnce(&mut DatabaseConnection) -> Result<R, Error>,
) -> Result<R, Error> {
    let lease = {
        let s = state.borrow();
        s.ensure_usable()?;
        s.lease.clone().ok_or_else(|| Error::Connection("connection is closed".into()))?
    };
    pool::with_connection(&lease, f)?
}

/// Release the lease of `state` (idempotent). Rolls back, then returns the slot to the pool
/// (persistent) or closes the physical connection (non-persistent); a broken lease is closed.
/// Also removes the `getInstance()` registry entry when this state owns it.
///
/// # Example
///
/// ```ignore
/// close_state(&state); // second call is a no-op
/// ```
pub fn close_state(state: &StateRef) {
    let (lease, broken, key, in_registry) = {
        let mut s = state.borrow_mut();
        if s.closed {
            return;
        }
        s.closed = true;
        s.epoch += 1;
        s.tx_level = 0;
        s.physical_tx_active = false;
        s.tx_failed = false;
        (
            s.lease.take(),
            s.broken,
            s.canonical_key.clone(),
            std::mem::replace(&mut s.in_registry, false),
        )
    };
    if let Some(lease) = lease {
        let _ = pool::release(lease, broken);
    }
    if in_registry {
        let removed = request::remove_if(&key, state);
        drop(removed);
    }
}

/// Replace a dead lease by a new one for the same DSN (`execute(reconnect: true)`).
///
/// The old lease is discarded as broken, a new one is acquired, the epoch is bumped
/// (older cursors become invalid). On failure the state is closed.
///
/// # Example
///
/// ```ignore
/// reconnect_state(&state)?; // only called when no transaction is active
/// ```
pub fn reconnect_state(state: &StateRef) -> Result<(), Error> {
    let (old, dsn_raw, persistent) = {
        let mut s = state.borrow_mut();
        (s.lease.take(), s.dsn_raw.clone(), s.persistent)
    };
    if let Some(l) = old {
        let _ = pool::release(l, true);
    }
    match pool::acquire(&dsn_raw, persistent) {
        Ok(lease) => {
            let mut s = state.borrow_mut();
            s.lease = Some(lease);
            s.epoch += 1;
            s.tx_level = 0;
            s.physical_tx_active = false;
            s.tx_failed = false;
            s.broken = false;
            s.clear_error();
            Ok(())
        }
        Err(e) => {
            close_state(state);
            Err(e)
        }
    }
}

/// `beginTransaction()` logic: level 1 sends nothing (PostgreSQL ensures `BEGIN`),
/// level >= 2 creates `SAVEPOINT RSDB_{slot}_{level}`.
pub fn begin_tx(state: &StateRef) -> Result<(), Error> {
    let (new_level, name, driver) = {
        let s = state.borrow();
        s.ensure_usable()?;
        if s.tx_failed {
            return Err(Error::Sql(
                "transaction is in a failed state; call rollBack()".into(),
            ));
        }
        let level = s.tx_level + 1;
        (level, s.savepoint_name(level), s.driver)
    };
    let _ = driver;
    run_on_connection(state, |c| {
        if new_level == 1 {
            c.begin()
        } else {
            c.savepoint(&name)
        }
    })
    .map_err(|e| note_failure(state, e))?;
    let mut s = state.borrow_mut();
    s.tx_level = new_level;
    s.physical_tx_active = true;
    s.clear_error();
    Ok(())
}

/// `commit()` logic: level <= 1 commits the physical transaction; level >= 2 releases the
/// savepoint (PostgreSQL) or only decrements the level (Oracle).
pub fn commit_tx(state: &StateRef) -> Result<(), Error> {
    let (level, name, driver) = {
        let s = state.borrow();
        s.ensure_usable()?;
        if s.tx_failed {
            return Err(Error::Sql(
                "cannot commit: transaction is in a failed state (25P02); call rollBack()".into(),
            ));
        }
        (s.tx_level, s.savepoint_name(s.tx_level), s.driver)
    };
    if level >= 2 {
        if driver == DriverKind::Pg {
            run_on_connection(state, |c| c.release_savepoint(&name))
                .map_err(|e| note_failure(state, e))?;
        }
        let mut s = state.borrow_mut();
        s.tx_level = level - 1;
        s.clear_error();
    } else {
        run_on_connection(state, |c| c.commit()).map_err(|e| note_failure(state, e))?;
        let mut s = state.borrow_mut();
        s.tx_level = 0;
        s.physical_tx_active = false;
        s.clear_error();
    }
    Ok(())
}

/// `rollBack()` logic: level <= 1 rolls back everything; level >= 2 rolls back to the savepoint
/// (+ release on PostgreSQL). Resets `tx_failed`. On a broken connection only the local
/// bookkeeping is reset (the lease is discarded by `close()`).
pub fn rollback_tx(state: &StateRef) -> Result<(), Error> {
    let (level, name) = {
        let mut s = state.borrow_mut();
        if s.closed || s.lease.is_none() {
            return Err(Error::Connection("connection is closed".into()));
        }
        if s.broken {
            s.tx_level = 0;
            s.physical_tx_active = false;
            s.tx_failed = false;
            return Ok(());
        }
        (s.tx_level, s.savepoint_name(s.tx_level))
    };
    if level >= 2 {
        run_on_connection(state, |c| c.rollback_to_savepoint(&name))
            .map_err(|e| note_failure(state, e))?;
        let mut s = state.borrow_mut();
        s.tx_level = level - 1;
        s.tx_failed = false;
        s.clear_error();
    } else {
        run_on_connection(state, |c| c.rollback()).map_err(|e| note_failure(state, e))?;
        let mut s = state.borrow_mut();
        s.tx_level = 0;
        s.physical_tx_active = false;
        s.tx_failed = false;
        s.clear_error();
    }
    Ok(())
}

/// Emit a PHP `E_WARNING`.
fn php_warning(msg: &str) {
    if let Ok(c) = CString::new(msg.replace('\0', " ")) {
        // SAFETY: valid NUL-terminated C strings; "%s" consumes exactly one argument.
        unsafe {
            ffi::php_error_docref(
                std::ptr::null(),
                ErrorType::Warning.bits() as i32,
                b"%s\0".as_ptr() as *const c_char,
                c.as_ptr(),
            );
        }
    }
}

/// Acquire a lease and build the shared state (no PHP object yet).
fn lease_new_state(address: &str, persistent: bool) -> Result<StateRef, Error> {
    let dsn = Dsn::parse(address)?;
    let lease = pool::acquire(address, persistent)?;
    let state = Rc::new(RefCell::new(ConnState::new(lease, &dsn, address, persistent)));
    request::track_state(&state);
    Ok(state)
}

/// Create the PHP object for a state; returns an owning zval (refcount 1).
fn new_connect_object(state: StateRef) -> PhpResult<Zval> {
    let obj = ZendClassObject::new(RsDataBaseConnect { state });
    obj.into_zval(false)
        .map_err(|e| usage_error(format!("cannot create RsDataBaseConnect object: {:?}", e)))
}

/// PHP class `RsDataBaseConnect` (final): factory, lease and transactions.
#[php_class]
#[php(name = "RsDataBaseConnect")]
#[php(flags = ClassFlags::Final)]
pub struct RsDataBaseConnect {
    pub(crate) state: StateRef,
}

impl Drop for RsDataBaseConnect {
    /// Last PHP reference gone == `close()` (idempotent; no-op after `RSHUTDOWN`).
    fn drop(&mut self) {
        let state = self.state.clone();
        let _ = catch_unwind(AssertUnwindSafe(|| close_state(&state)));
    }
}

#[php_impl]
impl RsDataBaseConnect {
    /// Private constructor; use `getInstance()` / `getNewInstance()`.
    ///
    /// ```php
    /// new RsDataBaseConnect(); // Error: Call to private RsDataBaseConnect::__construct()
    /// ```
    #[php(vis = "private")]
    pub fn __construct() -> PhpResult<Self> {
        Err(usage_error(
            "RsDataBaseConnect cannot be instantiated; use RsDataBaseConnect::getInstance()",
        ))
    }

    /// Request-wide shared connection: the same canonical DSN returns the same object (`===`).
    ///
    /// The `$persistent` flag of the first call wins; a differing flag on a later call emits
    /// `E_WARNING`. A broken connection without an active transaction is replaced transparently.
    ///
    /// ```php
    /// $db = RsDataBaseConnect::getInstance('pg://u:p@host:5432/db', true);
    /// $same = RsDataBaseConnect::getInstance('pg://u:p@host:5432/db');
    /// var_dump($db === $same); // true (+ E_WARNING: persistent flag differs)
    /// ```
    #[php(defaults(persistent = false))]
    pub fn get_instance(address: String, persistent: bool) -> PhpResult<Zval> {
        guard(|| {
            let dsn = Dsn::parse(&address)?;
            let key = dsn.canonical_key();

            if let Some((zval, state)) = request::lookup(&key) {
                let (replace, differs) = {
                    let s = state.borrow();
                    (
                        s.closed || (s.broken && s.tx_level == 0 && !s.physical_tx_active),
                        s.persistent != persistent,
                    )
                };
                if !replace {
                    if differs {
                        php_warning(
                            "RsDataBaseConnect::getInstance(): connection for this address already \
                             exists in this request; the $persistent flag of the first call is used",
                        );
                    }
                    return Ok(zval);
                }
                // Replace the broken lease with a fresh one.
                drop(zval);
                close_state(&state);
            }

            let state = lease_new_state(&address, persistent)?;
            state.borrow_mut().in_registry = true;
            let zval = new_connect_object(state.clone())?;
            let old = request::insert(key, zval.shallow_clone(), state);
            drop(old);
            Ok(zval)
        })
    }

    /// Always a new, independent lease and transaction; not part of the `getInstance()` registry.
    ///
    /// ```php
    /// $audit = RsDataBaseConnect::getNewInstance($dsn); // commits independently of $db
    /// ```
    #[php(defaults(persistent = false))]
    pub fn get_new_instance(address: String, persistent: bool) -> PhpResult<Zval> {
        guard(|| {
            let state = lease_new_state(&address, persistent)?;
            new_connect_object(state)
        })
    }

    /// Diagnostics of all connections of this process (no passwords).
    ///
    /// ```php
    /// print_r(RsDataBaseConnect::getConnections());
    /// // [['address'=>'pg://u:***@h:5432/db','status'=>'busy','persistent'=>true,'driver'=>'pg','idle_ms'=>null]]
    /// ```
    pub fn get_connections() -> PhpResult<Zval> {
        guard(|| {
            let items = pool::connection_snapshots()
                .into_iter()
                .map(|c| {
                    assoc_zval(vec![
                        ("address".into(), str_zval(&c.address)),
                        ("status".into(), str_zval(&c.status)),
                        ("persistent".into(), bool_zval(c.persistent)),
                        ("driver".into(), str_zval(&c.driver)),
                        (
                            "idle_ms".into(),
                            c.idle_ms.map(|v| long_zval(v as i64)).unwrap_or_else(Zval::null),
                        ),
                    ])
                })
                .collect();
            Ok(list_zval(items))
        })
    }

    /// Number of connections (persistent slots + non-persistent leases) in this process.
    ///
    /// ```php
    /// echo RsDataBaseConnect::getConnectionCount();
    /// ```
    pub fn get_connection_count() -> PhpResult<i64> {
        guard(|| Ok(pool::connection_snapshots().len() as i64))
    }

    /// Whether a driver is compiled in: `isSupported()` (any), `isSupported('pg')`, `isSupported('oci8')`.
    ///
    /// ```php
    /// RsDataBaseConnect::isSupported('pg'); // true when built with the postgres feature
    /// ```
    #[php(defaults(scheme = None))]
    pub fn is_supported(scheme: Option<String>) -> PhpResult<bool> {
        guard(|| {
            let pg = cfg!(feature = "postgres");
            let oci = cfg!(feature = "oracle");
            Ok(match scheme.as_deref().map(|s| s.to_ascii_lowercase()) {
                None => pg || oci,
                Some(s) if s == "pg" => pg,
                Some(s) if s == "oci8" => oci,
                Some(_) => false,
            })
        })
    }

    /// SQL parse-cache statistics: `entries`, `bytes`, `hits`, `misses`, `evictions`.
    ///
    /// ```php
    /// ['hits' => $h, 'misses' => $m] = RsDataBaseConnect::getParseCacheStats();
    /// ```
    pub fn get_parse_cache_stats() -> PhpResult<Zval> {
        guard(|| {
            let s = sql::stats();
            Ok(assoc_zval(vec![
                ("entries".into(), long_zval(s.entries as i64)),
                ("bytes".into(), long_zval(s.bytes as i64)),
                ("hits".into(), long_zval(s.hits as i64)),
                ("misses".into(), long_zval(s.misses as i64)),
                ("evictions".into(), long_zval(s.evictions as i64)),
            ]))
        })
    }

    /// Drop all cached SQL parse results of this process.
    ///
    /// ```php
    /// RsDataBaseConnect::clearParseCache();
    /// ```
    pub fn clear_parse_cache() -> PhpResult<()> {
        guard(|| {
            sql::clear();
            Ok(())
        })
    }

    /// `true` while the lease is open and not broken.
    ///
    /// ```php
    /// $db->isConnection(); // true
    /// $db->close();
    /// $db->isConnection(); // false
    /// ```
    pub fn is_connection(&self) -> bool {
        let s = self.state.borrow();
        !s.closed && !s.broken && s.lease.is_some()
    }

    /// DSN with the password masked as `***` (valid after `close()` too).
    ///
    /// ```php
    /// echo $db->getAddress(); // pg://DEV:***@192.168.241.137:5432/DEV
    /// ```
    pub fn get_address(&self) -> String {
        self.state.borrow().address.clone()
    }

    /// Driver name: `'pg'` or `'oci8'`.
    ///
    /// ```php
    /// echo $db->getDriver(); // pg
    /// ```
    pub fn get_driver(&self) -> String {
        self.state.borrow().driver.as_str().to_string()
    }

    /// Whether the lease was taken as persistent.
    ///
    /// ```php
    /// var_dump($db->isPersistent());
    /// ```
    pub fn is_persistent(&self) -> bool {
        self.state.borrow().persistent
    }

    /// Cheap liveness check; returns `false` for a closed or broken connection.
    ///
    /// ```php
    /// if (!$db->ping()) { $db->close(); }
    /// ```
    pub fn ping(&self) -> PhpResult<bool> {
        guard(|| {
            {
                let s = self.state.borrow();
                if s.closed || s.broken || s.lease.is_none() {
                    return Ok(false);
                }
            }
            match run_on_connection(&self.state, |c| c.ping()) {
                Ok(v) => Ok(v),
                Err(e) => {
                    self.state.borrow_mut().set_error(&e);
                    Ok(false)
                }
            }
        })
    }

    /// Create a statement. SQL that only controls transactions (`BEGIN;`, `COMMIT`, ...) is rejected.
    ///
    /// ```php
    /// $q = $db->prepare('SELECT id FROM t WHERE name = :name');
    /// ```
    pub fn prepare<'a>(
        self_: &'a mut ZendClassObject<RsDataBaseConnect>,
        sql: String,
    ) -> PhpResult<ext_php_rs::boxed::ZBox<ZendClassObject<RsDataBasePrepare>>> {
        guard(|| {
            let state = self_.state.clone();
            state.borrow().ensure_usable()?;
            let mut conn_obj = Zval::new();
            conn_obj.set_object(&mut self_.std);
            let prepare = RsDataBasePrepare::create(conn_obj, state, sql)?;
            Ok(ZendClassObject::new(prepare))
        })
    }

    /// Start a transaction (level 1) or a savepoint (level >= 2). Returns `$this`.
    ///
    /// ```php
    /// $db->beginTransaction();
    /// $db->beginTransaction(); // SAVEPOINT RSDB_<slot>_2
    /// ```
    pub fn begin_transaction<'a>(
        self_: &'a mut ZendClassObject<RsDataBaseConnect>,
    ) -> PhpResult<&'a mut ZendClassObject<RsDataBaseConnect>> {
        guard(|| {
            begin_tx(&self_.state)?;
            Ok(())
        })?;
        Ok(self_)
    }

    /// Commit the transaction (level <= 1) or release the savepoint (level >= 2).
    /// Throws `RsDataBaseException` if the PostgreSQL transaction is in the failed state.
    ///
    /// ```php
    /// $db->commit();
    /// ```
    pub fn commit<'a>(self_: &'a mut ZendClassObject<RsDataBaseConnect>) -> PhpResult<&'a mut ZendClassObject<RsDataBaseConnect>> {
        guard(|| {
            commit_tx(&self_.state)?;
            Ok(())
        })?;
        Ok(self_)
    }

    /// Roll back everything (level <= 1) or to the savepoint (level >= 2). Resets the failed state.
    ///
    /// ```php
    /// $db->rollBack();
    /// ```
    pub fn roll_back<'a>(self_: &'a mut ZendClassObject<RsDataBaseConnect>) -> PhpResult<&'a mut ZendClassObject<RsDataBaseConnect>> {
        guard(|| {
            rollback_tx(&self_.state)?;
            Ok(())
        })?;
        Ok(self_)
    }

    /// `transaction_level > 0` (an implicit transaction after a bare `SELECT` does not count).
    ///
    /// ```php
    /// var_dump($db->inTransaction());
    /// ```
    pub fn in_transaction(&self) -> bool {
        self.state.borrow().tx_level > 0
    }

    /// Current explicit transaction nesting level.
    ///
    /// ```php
    /// echo $db->getTransactionLevel(); // 0, 1, 2, ...
    /// ```
    pub fn get_transaction_level(&self) -> i64 {
        i64::from(self.state.borrow().tx_level)
    }

    /// Roll back, release the lease and remove the object from the `getInstance()` registry.
    /// Idempotent; affects every holder of the object. Child statements become invalid.
    ///
    /// ```php
    /// $db->close();
    /// $db->close(); // no-op
    /// ```
    pub fn close(&self) -> PhpResult<()> {
        guard(|| {
            close_state(&self.state);
            Ok(())
        })
    }

    /// Last error code (SQLSTATE / ORA-xxxxx) or `null`.
    ///
    /// ```php
    /// echo $db->getErrorCode();
    /// ```
    pub fn get_error_code(&self) -> Option<String> {
        self.state.borrow().error_code.clone()
    }

    /// Last error message or `null`.
    ///
    /// ```php
    /// echo $db->getErrorMessage();
    /// ```
    pub fn get_error_message(&self) -> Option<String> {
        self.state.borrow().error_message.clone()
    }
}

/// Convenience used by statements: error for a closed/stale lease.
pub fn stale_lease_error() -> ext_php_rs::exception::PhpException {
    connection_error("connection was closed or replaced; statement is no longer valid")
}
