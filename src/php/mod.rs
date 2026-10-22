//! PHP bindings (`ext-php-rs`) for the `rs_database_connect` core.
//!
//! Layout:
//!
//! | Module        | Responsibility                                                       |
//! | ------------- | -------------------------------------------------------------------- |
//! | `exception`   | PHP exception classes and `Error` → `PhpException` mapping            |
//! | `convert`     | `Zval` ↔ `BindValue`/`DbValue` helpers, `catch_unwind` guard          |
//! | `request`     | Per-request registry (`getInstance()` objects, orphans, binds)        |
//! | `connect`     | `RsDataBaseConnect` (factory, lease, transactions)                    |
//! | `prepare`     | `RsDataBasePrepare` (SQL, binds, execute, fetch)                      |
//! | `bind`        | `RsDataBaseBind` (bind parameter object)                              |
//! | `cursor`      | `RsDataBaseCursor` (OUT cursor result) and the shared row cursor      |
//! | `lifecycle`   | `MINIT` / `RINIT` / `RSHUTDOWN` / `MSHUTDOWN`, INI registration      |
//!
//! Everything in this module tree runs on the PHP thread only.

pub mod bind;
pub mod connect;
pub mod convert;
pub mod cursor;
pub mod exception;
pub mod lifecycle;
pub mod prepare;
pub mod request;
