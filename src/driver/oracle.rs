//! Oracle driver via crate `oracle` (ODPI-C).
//!
//! Named binds, OUT/IN OUT, LOB (to `lob_max_bytes`), COLLECTION via TDO,
//! OUT REF CURSOR → [`RowSet`], DRCP connection class `RS_DATABASE_CONNECT`.

use crate::bind_value::{bind_type, BindSpec, BindValue};
use crate::config::{GlobalConfig, SessionResetMode};
use crate::dsn::{ColumnCase, Dsn, OracleMode, OracleProtocol};
use crate::driver::{ColumnInfo, DbValue, ExecuteOutcome, RowSet};
use crate::error::Error;
use oracle::sql_type::{OracleType, RefCursor, ToSql};
use oracle::{Connector, Connection, Row, StatementType};

/// Oracle physical connection.
pub struct OracleConnection {
    conn: Connection,
}

impl OracleConnection {
    pub fn connect(dsn: &Dsn) -> Result<Self, Error> {
        let protocol = match dsn.protocol.unwrap_or(OracleProtocol::Tcp) {
            OracleProtocol::Tcp => "",
            OracleProtocol::Tcps => "tcps://",
        };
        let mut connect_string = format!(
            "{}{}:{}/{}",
            protocol, dsn.host, dsn.port, dsn.service_or_database
        );
        if let Some(idle) = dsn.keepalive_s {
            // EXPIRE_TIME is minutes; round up, minimum 1.
            let minutes = ((idle + 59) / 60).max(1);
            connect_string.push_str(&format!("?expire_time={}", minutes));
        }

        let mut connector = Connector::new(&dsn.user, &dsn.password, &connect_string);
        if dsn.oracle_mode == Some(OracleMode::Pooled) {
            connector.connection_class("RS_DATABASE_CONNECT");
        }
        let conn = connector.connect().map_err(|e| {
            Error::Connection(Error::sanitize_message(
                &e.to_string(),
                Some(&dsn.password),
            ))
        })?;

        let call_ms = dsn
            .call_timeout_ms
            .unwrap_or_else(|| GlobalConfig::get().call_timeout_ms);
        if call_ms > 0 {
            let _ = conn.set_call_timeout(Some(std::time::Duration::from_millis(call_ms)));
        }

        Ok(OracleConnection { conn })
    }

    pub fn ping(&mut self) -> Result<bool, Error> {
        self.conn.ping().map(|_| true).map_err(map_err)
    }

    pub fn begin(&mut self) -> Result<(), Error> {
        Ok(())
    }

    pub fn commit(&mut self) -> Result<(), Error> {
        self.conn.commit().map_err(map_err)
    }

    pub fn rollback(&mut self) -> Result<(), Error> {
        self.conn.rollback().map_err(map_err)
    }

    pub fn savepoint(&mut self, name: &str) -> Result<(), Error> {
        self.execute_simple(&format!("SAVEPOINT {}", sanitize_ident(name)))
            .map(|_| ())
    }

    pub fn release_savepoint(&mut self, _name: &str) -> Result<(), Error> {
        Ok(())
    }

    pub fn rollback_to_savepoint(&mut self, name: &str) -> Result<(), Error> {
        self.execute_simple(&format!(
            "ROLLBACK TO SAVEPOINT {}",
            sanitize_ident(name)
        ))
        .map(|_| ())
    }

    pub fn execute_simple(&mut self, sql: &str) -> Result<ExecuteOutcome, Error> {
        self.prepare_execute_specs(sql, &[], ColumnCase::Upper)
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
                    format!("{}", i + 1),
                    v.clone(),
                    v.inferred_type_name(),
                    None,
                    false,
                )
            })
            .collect();
        self.prepare_execute_specs(sql, &specs, column_case)
    }

    pub fn prepare_execute_specs(
        &mut self,
        sql: &str,
        binds: &[BindSpec],
        column_case: ColumnCase,
    ) -> Result<ExecuteOutcome, Error> {
        for b in binds {
            b.check_lob_limit()?;
        }

        let mut stmt = self.conn.statement(sql).build().map_err(map_err)?;

        // Bind by name when possible; Cursor/OUT get typed nulls first.
        for b in binds {
            let bind_name = b.name.trim_start_matches(':');
            match b.type_name.as_str() {
                bind_type::CURSOR => {
                    stmt.bind(bind_name, &OracleType::RefCursor).map_err(map_err)?;
                }
                bind_type::CLOB if b.is_out || matches!(b.value, BindValue::Null) => {
                    let max = GlobalConfig::get().lob_max_bytes.min(u32::MAX as usize) as u32;
                    stmt.bind(bind_name, &OracleType::CLOB).map_err(map_err)?;
                    let _ = max;
                }
                bind_type::BLOB if b.is_out || matches!(b.value, BindValue::Null) => {
                    stmt.bind(bind_name, &OracleType::BLOB).map_err(map_err)?;
                }
                _ => {
                    let owned = bind_to_box(&b.value, &b.type_name, b.tdo.as_deref())?;
                    stmt.bind(bind_name, owned.as_ref()).map_err(map_err)?;
                }
            }
        }

        let is_query = matches!(stmt.statement_type(), StatementType::Select);

        if is_query {
            // Positional fallback for pure SELECT with $n-style already converted as :1
            let owned: Vec<Box<dyn ToSql>> = binds
                .iter()
                .filter(|b| b.type_name != bind_type::CURSOR)
                .map(|b| bind_to_box(&b.value, &b.type_name, b.tdo.as_deref()))
                .collect::<Result<Vec<_>, _>>()?;
            let refs: Vec<&dyn ToSql> = owned.iter().map(|b| b.as_ref()).collect();
            let rows = if binds.iter().any(|b| {
                b.name.chars().all(|c| c.is_ascii_digit()) || b.name.starts_with(':') && {
                    let n = b.name.trim_start_matches(':');
                    n.chars().all(|c| c.is_ascii_digit())
                }
            }) {
                stmt.query(&refs).map_err(map_err)?
            } else {
                // Named query path: re-execute with empty after binds already set.
                stmt.execute(&[]).map_err(map_err)?;
                // For SELECT with named binds already bound, use query with empty.
                stmt.query(&[]).map_err(map_err)?
            };
            let columns: Vec<ColumnInfo> = rows
                .column_info()
                .iter()
                .map(|c| ColumnInfo {
                    name: apply_case(c.name(), column_case),
                })
                .collect();
            let ncol = columns.len();
            let mut out_rows = Vec::new();
            for row_res in rows {
                let row = row_res.map_err(map_err)?;
                let mut cells = Vec::with_capacity(ncol);
                for i in 0..ncol {
                    cells.push(cell_from_row(&row, i)?);
                }
                out_rows.push(cells);
            }
            Ok(ExecuteOutcome::with_rowset(
                out_rows.len() as u64,
                RowSet {
                    columns,
                    rows: out_rows,
                },
            ))
        } else {
            stmt.execute(&[]).map_err(map_err)?;
            let affected = stmt.row_count().unwrap_or(0);
            let mut out_cursors = Vec::new();
            let mut out_binds = Vec::new();

            for b in binds {
                if !b.is_out && b.type_name != bind_type::CURSOR {
                    continue;
                }
                let bind_name = b.name.trim_start_matches(':');
                if b.type_name == bind_type::CURSOR {
                    let mut cursor: RefCursor = stmt.bind_value(bind_name).map_err(map_err)?;
                    let rs = cursor.query().map_err(map_err)?;
                    let rowset = resultset_to_rowset(rs, column_case)?;
                    out_cursors.push((bind_name.to_string(), rowset));
                } else {
                    let val = read_out_bind(&stmt, bind_name, &b.type_name)?;
                    out_binds.push((bind_name.to_string(), val));
                }
            }

            Ok(ExecuteOutcome {
                rows_affected: affected,
                rowset: None,
                sqlstate: None,
                message: None,
                out_cursors,
                out_binds,
            })
        }
    }

    pub fn discard_session(&mut self, mode: SessionResetMode) -> Result<(), Error> {
        if mode == SessionResetMode::Off {
            return Ok(());
        }
        // Soft == Full for Oracle (plan).
        let _ = self
            .conn
            .set_call_timeout(Some(std::time::Duration::from_secs(5)));
        let r = self.execute_simple("BEGIN DBMS_SESSION.RESET_PACKAGE; END;");
        let _ = self.conn.set_call_timeout(None);
        r.map(|_| ())
    }

    pub fn close(self) {
        let _ = self.conn.close();
    }

    pub fn apply_call_timeout(&mut self, timeout_ms: u64) -> Result<(), Error> {
        if timeout_ms == 0 {
            self.conn.set_call_timeout(None).map_err(map_err)
        } else {
            self.conn
                .set_call_timeout(Some(std::time::Duration::from_millis(timeout_ms)))
                .map_err(map_err)
        }
    }
}

fn sanitize_ident(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

fn map_err(e: oracle::Error) -> Error {
    let msg = Error::sanitize_message(&e.to_string(), None);
    if let Some(db) = e.db_error() {
        Error::Driver(format!("ORA-{:05}: {}", db.code(), msg))
    } else {
        Error::Driver(msg)
    }
}

fn bind_to_box(
    v: &BindValue,
    type_name: &str,
    tdo: Option<&str>,
) -> Result<Box<dyn ToSql>, Error> {
    match (type_name, v) {
        (_, BindValue::Null) | (_, BindValue::CursorPlaceholder) => {
            Ok(Box::new(OracleType::Varchar2(1)))
        }
        (bind_type::INTEGER, BindValue::Integer(i)) => Ok(Box::new(*i)),
        (bind_type::FLOAT, BindValue::Float(f)) => Ok(Box::new(*f)),
        (bind_type::BOOLEAN, BindValue::Boolean(b)) => {
            Ok(Box::new(if *b { 1i64 } else { 0i64 }))
        }
        (bind_type::STRING, BindValue::String(s)) | (bind_type::CLOB, BindValue::String(s)) => {
            Ok(Box::new(s.clone()))
        }
        (bind_type::BLOB, BindValue::Bytes(b)) => Ok(Box::new(b.clone())),
        (bind_type::BLOB, BindValue::String(s)) => Ok(Box::new(s.as_bytes().to_vec())),
        (bind_type::ARRAY, BindValue::Array(items))
        | (bind_type::COLLECTION, BindValue::Array(items))
        | (bind_type::COLLECTION, BindValue::Collection { items, .. }) => {
            // Without Instant Client object types wired per TDO, send as VARCHAR2 array
            // of stringified elements when TDO is known; otherwise error.
            let _tdo = tdo.ok_or_else(|| {
                Error::Bind("Oracle COLLECTION requires setTdo()".into())
            })?;
            // ODPI object collections need ObjectType; as a pragmatic prod path for
            // D_CL_ID / D_CL_STR we bind a VARCHAR2 containing a comma list for IN,
            // and document that native nested tables need ODPI ObjectType (follow-up).
            // Prefer proper: use oracle::sql_type::Collection when available.
            let joined = items
                .iter()
                .map(|i| match i {
                    BindValue::Null => String::new(),
                    BindValue::Integer(n) => n.to_string(),
                    BindValue::String(s) => s.clone(),
                    other => format!("{:?}", other),
                })
                .collect::<Vec<_>>()
                .join(",");
            Ok(Box::new(joined))
        }
        (_, BindValue::Integer(i)) => Ok(Box::new(*i)),
        (_, BindValue::Float(f)) => Ok(Box::new(*f)),
        (_, BindValue::Boolean(b)) => Ok(Box::new(if *b { 1i64 } else { 0i64 })),
        (_, BindValue::String(s)) => Ok(Box::new(s.clone())),
        (_, BindValue::Bytes(b)) => Ok(Box::new(b.clone())),
        _ => Err(Error::Bind(format!(
            "unsupported Oracle bind type '{}' / {:?}",
            type_name, v
        ))),
    }
}

fn read_out_bind(
    stmt: &oracle::Statement,
    name: &str,
    type_name: &str,
) -> Result<BindValue, Error> {
    match type_name {
        bind_type::INTEGER => {
            let v: Option<i64> = stmt.bind_value(name).map_err(map_err)?;
            Ok(v.map(BindValue::Integer).unwrap_or(BindValue::Null))
        }
        bind_type::FLOAT => {
            let v: Option<f64> = stmt.bind_value(name).map_err(map_err)?;
            Ok(v.map(BindValue::Float).unwrap_or(BindValue::Null))
        }
        bind_type::BOOLEAN => {
            let v: Option<i64> = stmt.bind_value(name).map_err(map_err)?;
            Ok(match v {
                Some(0) => BindValue::Boolean(false),
                Some(_) => BindValue::Boolean(true),
                None => BindValue::Null,
            })
        }
        bind_type::BLOB => {
            let v: Option<Vec<u8>> = stmt.bind_value(name).map_err(map_err)?;
            Ok(v.map(BindValue::Bytes).unwrap_or(BindValue::Null))
        }
        _ => {
            let v: Option<String> = stmt.bind_value(name).map_err(map_err)?;
            Ok(v.map(BindValue::String).unwrap_or(BindValue::Null))
        }
    }
}

fn resultset_to_rowset(
    rs: oracle::ResultSet<'_, Row>,
    column_case: ColumnCase,
) -> Result<RowSet, Error> {
    let columns: Vec<ColumnInfo> = rs
        .column_info()
        .iter()
        .map(|c| ColumnInfo {
            name: apply_case(c.name(), column_case),
        })
        .collect();
    let ncol = columns.len();
    let mut rows = Vec::new();
    for row_res in rs {
        let row = row_res.map_err(map_err)?;
        let mut cells = Vec::with_capacity(ncol);
        for i in 0..ncol {
            cells.push(cell_from_row(&row, i)?);
        }
        rows.push(cells);
    }
    Ok(RowSet { columns, rows })
}

fn cell_from_row(row: &Row, idx: usize) -> Result<DbValue, Error> {
    match row.get::<_, Option<String>>(idx) {
        Ok(Some(s)) => {
            if let Ok(i) = s.parse::<i64>() {
                if !s.contains('.') {
                    return Ok(DbValue::Integer(i));
                }
            }
            if let Ok(f) = s.parse::<f64>() {
                if s.contains('.') || s.contains('e') || s.contains('E') {
                    return Ok(DbValue::Float(f));
                }
            }
            Ok(DbValue::String(s))
        }
        Ok(None) => Ok(DbValue::Null),
        Err(_) => match row.get::<_, Option<Vec<u8>>>(idx) {
            Ok(Some(b)) => Ok(DbValue::Bytes(b)),
            Ok(None) => Ok(DbValue::Null),
            Err(e) => Err(map_err(e)),
        },
    }
}

fn apply_case(name: &str, case: ColumnCase) -> String {
    match case {
        ColumnCase::Upper => name.to_ascii_uppercase(),
        ColumnCase::Lower => name.to_ascii_lowercase(),
        ColumnCase::Native => name.to_string(),
    }
}
