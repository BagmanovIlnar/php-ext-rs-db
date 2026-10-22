//! PHP exception classes and mapping from the core [`Error`] type.
//!
//! Hierarchy:
//!
//! ```text
//! \Exception
//! └── RsDataBaseException
//!     ├── RsDataBaseConnectionException   (connect failure, closed/broken connection)
//!     ├── RsDataBasePoolExhaustedException (pool is full, waiting is pointless - NTS)
//!     └── RsDataBasePoolTimeoutException   (pool_wait_timeout_ms elapsed - ZTS)
//! ```
//!
//! Classes are registered by [`register_classes`] (not `#[php_class]`) so file/line/trace work.
//!
//! `RsDataBaseException` is the only non-`final` class: the three specialised
//! exceptions extend it, so PHP code can `catch (RsDataBaseException $e)`.
//!
//! # Example (PHP)
//!
//! ```php
//! try {
//!     $db = RsDataBaseConnect::getInstance('pg://u:p@host/db');
//! } catch (RsDataBasePoolExhaustedException $e) {
//!     // too many leases for this DSN in this process
//! } catch (RsDataBaseConnectionException $e) {
//!     // cannot connect
//! } catch (RsDataBaseException $e) {
//!     // everything else (bad DSN, bad SQL usage, ...)
//! }
//! ```

use crate::error::Error;
use std::sync::atomic::{AtomicPtr, Ordering};
use ext_php_rs::exception::PhpException;
use ext_php_rs::flags::ClassFlags;
use ext_php_rs::builders::ClassBuilder;
use ext_php_rs::zend::{ce, ClassEntry};

/// Names of the registered PHP exception classes.
pub const BASE: &str = "RsDataBaseException";
/// See [`register_classes`].
pub const CONNECTION: &str = "RsDataBaseConnectionException";
/// See [`register_classes`].
pub const POOL_EXHAUSTED: &str = "RsDataBasePoolExhaustedException";
/// See [`register_classes`].
pub const POOL_TIMEOUT: &str = "RsDataBasePoolTimeoutException";

/// Class entry of `RsDataBaseException`, stored at registration (class lookup by name
/// does not work yet during `MINIT`).
static BASE_CE: AtomicPtr<ClassEntry> = AtomicPtr::new(std::ptr::null_mut());

fn base_ce() -> &'static ClassEntry {
    let p = BASE_CE.load(Ordering::Acquire);
    if p.is_null() {
        ce::exception()
    } else {
        // SAFETY: pointer comes from zend_register_internal_class_ex and lives for the process.
        unsafe { &*p }
    }
}

/// Register the four exception classes (called from `MINIT`).
///
/// The classes are registered with `ClassBuilder` *without* a Rust-backed object, so they
/// inherit `\Exception`'s object constructor. That is what fills `file`, `line` and `trace`
/// of the exception (a `#[php_class]` exception would report line 0 / no trace).
///
/// - `RsDataBaseException` - base class, not final.
/// - `RsDataBaseConnectionException`, `RsDataBasePoolExhaustedException`,
///   `RsDataBasePoolTimeoutException` - `final`, extend the base class.
///
/// # Example (PHP)
///
/// ```php
/// try { RsDataBaseConnect::getInstance('bad'); }
/// catch (RsDataBaseException $e) { echo $e->getFile(), ':', $e->getLine(); }
/// ```
pub fn register_classes() -> Result<(), String> {
    ClassBuilder::new(BASE)
        .extends((ce::exception, "\\Exception"))
        .registration(|ce| BASE_CE.store(ce as *mut ClassEntry, Ordering::Release))
        .register()
        .map_err(|e| format!("cannot register {}: {:?}", BASE, e))?;
    for name in [CONNECTION, POOL_EXHAUSTED, POOL_TIMEOUT] {
        ClassBuilder::new(name)
            .extends((base_ce, BASE))
            .flags(ClassFlags::Final)
            .registration(|_| {})
            .register()
            .map_err(|e| format!("cannot register {}: {:?}", name, e))?;
    }
    Ok(())
}

/// Build an exception of the registered class `name` (falls back to `\Exception`).
fn exception_of(name: &str, msg: String) -> PhpException {
    let ce = ClassEntry::try_find(name).unwrap_or_else(ce::exception);
    PhpException::new(msg, 0, ce)
}

/// Message without the `Display` prefix (`"Driver error: "`, ...), as shown to PHP users.
///
/// # Example
///
/// ```ignore
/// assert_eq!(error_message(&Error::Sql("bad".into())), "bad");
/// ```
pub fn error_message(e: &Error) -> String {
    match e {
        Error::Dsn(m)
        | Error::Connection(m)
        | Error::Sql(m)
        | Error::Bind(m)
        | Error::Driver(m)
        | Error::Internal(m) => m.clone(),
        Error::PoolExhausted | Error::PoolTimeout => e.to_string(),
    }
}

/// Split a driver error into `(SQLSTATE or ORA code, message)`.
///
/// The PostgreSQL driver formats errors as `"[42P01] relation ... does not exist"`.
///
/// # Example
///
/// ```ignore
/// let (code, msg) = error_parts(&Error::Driver("[42P01] no table".into()));
/// assert_eq!(code.as_deref(), Some("42P01"));
/// assert_eq!(msg, "no table");
/// ```
pub fn error_parts(e: &Error) -> (Option<String>, String) {
    let msg = error_message(e);
    if let Error::Driver(m) = e {
        if let Some(rest) = m.strip_prefix('[') {
            if let Some((code, tail)) = rest.split_once("] ") {
                if !code.is_empty() && code.len() <= 16 && !code.contains(' ') {
                    return (Some(code.to_string()), tail.to_string());
                }
            }
        }
    }
    (None, msg)
}

impl From<Error> for PhpException {
    /// Map a core error to the matching PHP exception class.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let ex: PhpException = Error::PoolExhausted.into(); // RsDataBasePoolExhaustedException
    /// ```
    fn from(e: Error) -> Self {
        let msg = error_message(&e);
        match e {
            Error::Connection(_) => exception_of(CONNECTION, msg),
            Error::PoolExhausted => exception_of(POOL_EXHAUSTED, msg),
            Error::PoolTimeout => exception_of(POOL_TIMEOUT, msg),
            _ => exception_of(BASE, msg),
        }
    }
}

/// Shortcut: plain `RsDataBaseException` with the given message.
///
/// # Example
///
/// ```ignore
/// return Err(usage_error("execute() must be called before fetchRow()"));
/// ```
pub fn usage_error(msg: impl Into<String>) -> PhpException {
    exception_of(BASE, msg.into())
}

/// Shortcut: `RsDataBaseConnectionException` with the given message.
///
/// # Example
///
/// ```ignore
/// return Err(connection_error("connection is closed"));
/// ```
pub fn connection_error(msg: impl Into<String>) -> PhpException {
    exception_of(CONNECTION, msg.into())
}
