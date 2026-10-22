//! Database drivers (PostgreSQL, Oracle).

#[cfg(feature = "postgres")]
pub mod pg_codec;
#[cfg(feature = "postgres")]
pub mod postgres;

#[cfg(feature = "oracle")]
pub mod oracle;

use crate::bind_value::{BindSpec, BindValue};
use crate::config::SessionResetMode;
use crate::dsn::{ColumnCase, Dsn, DriverKind};
use crate::error::Error;

/// One cell in a fetched row.
#[derive(Debug, Clone, PartialEq)]
pub enum DbValue {
    Null,
    Integer(i64),
    Float(f64),
    Boolean(bool),
    String(String),
    Bytes(Vec<u8>),
    /// One-dimensional array (PostgreSQL arrays).
    Array(Vec<DbValue>),
}

/// Column metadata for a result set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
}

/// In-memory result of a query (all rows fetched).
#[derive(Debug, Clone, PartialEq)]
pub struct RowSet {
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<Vec<DbValue>>,
}

/// Outcome of a statement that may not return rows.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecuteOutcome {
    pub rows_affected: u64,
    pub rowset: Option<RowSet>,
    pub sqlstate: Option<String>,
    pub message: Option<String>,
    /// OUT CURSOR / refcursor results keyed by bind name (without leading `:`).
    pub out_cursors: Vec<(String, RowSet)>,
    /// Scalar OUT/IN OUT values keyed by bind name (Oracle named OUT, etc.).
    pub out_binds: Vec<(String, BindValue)>,
}

impl ExecuteOutcome {
    pub fn empty_ok(rows_affected: u64) -> Self {
        Self {
            rows_affected,
            rowset: None,
            sqlstate: None,
            message: None,
            out_cursors: Vec::new(),
            out_binds: Vec::new(),
        }
    }

    pub fn with_rowset(rows_affected: u64, rowset: RowSet) -> Self {
        Self {
            rows_affected,
            rowset: Some(rowset),
            sqlstate: None,
            message: None,
            out_cursors: Vec::new(),
            out_binds: Vec::new(),
        }
    }
}

/// Active physical connection (driver-specific).
pub enum DatabaseConnection {
    #[cfg(feature = "postgres")]
    Postgres(postgres::PgConnection),
    #[cfg(feature = "oracle")]
    Oracle(oracle::OracleConnection),
}

impl DatabaseConnection {
    /// Open a connection from a parsed DSN.
    pub fn connect(dsn: &Dsn) -> Result<Self, Error> {
        match dsn.driver {
            #[cfg(feature = "postgres")]
            DriverKind::Pg => Ok(DatabaseConnection::Postgres(
                postgres::PgConnection::connect(dsn)?,
            )),
            #[cfg(not(feature = "postgres"))]
            DriverKind::Pg => Err(Error::Driver("postgres feature disabled".into())),
            #[cfg(feature = "oracle")]
            DriverKind::Oci8 => Ok(DatabaseConnection::Oracle(
                oracle::OracleConnection::connect(dsn)?,
            )),
            #[cfg(not(feature = "oracle"))]
            DriverKind::Oci8 => Err(Error::Driver("oracle feature disabled".into())),
        }
    }

    pub fn driver_kind(&self) -> DriverKind {
        match self {
            #[cfg(feature = "postgres")]
            DatabaseConnection::Postgres(_) => DriverKind::Pg,
            #[cfg(feature = "oracle")]
            DatabaseConnection::Oracle(_) => DriverKind::Oci8,
        }
    }

    pub fn ping(&mut self) -> Result<bool, Error> {
        match self {
            #[cfg(feature = "postgres")]
            DatabaseConnection::Postgres(c) => c.ping(),
            #[cfg(feature = "oracle")]
            DatabaseConnection::Oracle(c) => c.ping(),
        }
    }

    pub fn is_valid(&mut self) -> Result<bool, Error> {
        self.ping()
    }

    pub fn begin(&mut self) -> Result<(), Error> {
        match self {
            #[cfg(feature = "postgres")]
            DatabaseConnection::Postgres(c) => c.begin(),
            #[cfg(feature = "oracle")]
            DatabaseConnection::Oracle(c) => c.begin(),
        }
    }

    pub fn commit(&mut self) -> Result<(), Error> {
        match self {
            #[cfg(feature = "postgres")]
            DatabaseConnection::Postgres(c) => c.commit(),
            #[cfg(feature = "oracle")]
            DatabaseConnection::Oracle(c) => c.commit(),
        }
    }

    pub fn rollback(&mut self) -> Result<(), Error> {
        match self {
            #[cfg(feature = "postgres")]
            DatabaseConnection::Postgres(c) => c.rollback(),
            #[cfg(feature = "oracle")]
            DatabaseConnection::Oracle(c) => c.rollback(),
        }
    }

    pub fn savepoint(&mut self, name: &str) -> Result<(), Error> {
        match self {
            #[cfg(feature = "postgres")]
            DatabaseConnection::Postgres(c) => c.savepoint(name),
            #[cfg(feature = "oracle")]
            DatabaseConnection::Oracle(c) => c.savepoint(name),
        }
    }

    pub fn release_savepoint(&mut self, name: &str) -> Result<(), Error> {
        match self {
            #[cfg(feature = "postgres")]
            DatabaseConnection::Postgres(c) => c.release_savepoint(name),
            #[cfg(feature = "oracle")]
            DatabaseConnection::Oracle(c) => c.release_savepoint(name),
        }
    }

    pub fn rollback_to_savepoint(&mut self, name: &str) -> Result<(), Error> {
        match self {
            #[cfg(feature = "postgres")]
            DatabaseConnection::Postgres(c) => c.rollback_to_savepoint(name),
            #[cfg(feature = "oracle")]
            DatabaseConnection::Oracle(c) => c.rollback_to_savepoint(name),
        }
    }

    pub fn execute_simple(&mut self, sql: &str) -> Result<ExecuteOutcome, Error> {
        match self {
            #[cfg(feature = "postgres")]
            DatabaseConnection::Postgres(c) => c.execute_simple(sql),
            #[cfg(feature = "oracle")]
            DatabaseConnection::Oracle(c) => c.execute_simple(sql),
        }
    }

    pub fn prepare_execute(
        &mut self,
        sql: &str,
        binds: &[BindValue],
        column_case: ColumnCase,
    ) -> Result<ExecuteOutcome, Error> {
        let specs: Vec<BindSpec> = binds
            .iter()
            .enumerate()
            .map(|(i, v)| {
                BindSpec::new(
                    format!("p{}", i + 1),
                    v.clone(),
                    v.inferred_type_name(),
                    None,
                    false,
                )
            })
            .collect();
        self.prepare_execute_specs(sql, &specs, column_case)
    }

    /// Execute with full bind metadata (names, TDO, OUT flags, cursor).
    pub fn prepare_execute_specs(
        &mut self,
        sql: &str,
        binds: &[BindSpec],
        column_case: ColumnCase,
    ) -> Result<ExecuteOutcome, Error> {
        for b in binds {
            b.check_lob_limit()?;
        }
        match self {
            #[cfg(feature = "postgres")]
            DatabaseConnection::Postgres(c) => c.prepare_execute_specs(sql, binds, column_case),
            #[cfg(feature = "oracle")]
            DatabaseConnection::Oracle(c) => c.prepare_execute_specs(sql, binds, column_case),
        }
    }

    pub fn discard_session(&mut self, mode: SessionResetMode) -> Result<(), Error> {
        match self {
            #[cfg(feature = "postgres")]
            DatabaseConnection::Postgres(c) => c.discard_session(mode),
            #[cfg(feature = "oracle")]
            DatabaseConnection::Oracle(c) => c.discard_session(mode),
        }
    }

    pub fn close(self) {
        match self {
            #[cfg(feature = "postgres")]
            DatabaseConnection::Postgres(c) => c.close(),
            #[cfg(feature = "oracle")]
            DatabaseConnection::Oracle(c) => c.close(),
        }
    }

    pub fn apply_call_timeout(&mut self, timeout_ms: u64) -> Result<(), Error> {
        match self {
            #[cfg(feature = "postgres")]
            DatabaseConnection::Postgres(c) => c.apply_call_timeout(timeout_ms),
            #[cfg(feature = "oracle")]
            DatabaseConnection::Oracle(c) => c.apply_call_timeout(timeout_ms),
        }
    }

    /// Materialize a PostgreSQL refcursor portal into a [`RowSet`].
    pub fn fetch_refcursor(
        &mut self,
        portal_name: &str,
        column_case: ColumnCase,
    ) -> Result<RowSet, Error> {
        match self {
            #[cfg(feature = "postgres")]
            DatabaseConnection::Postgres(c) => c.fetch_refcursor(portal_name, column_case),
            #[cfg(feature = "oracle")]
            DatabaseConnection::Oracle(_) => Err(Error::Driver(
                "fetch_refcursor is PostgreSQL-only".into(),
            )),
            #[cfg(not(any(feature = "postgres", feature = "oracle")))]
            _ => Err(Error::Driver("no driver".into())),
        }
    }
}
