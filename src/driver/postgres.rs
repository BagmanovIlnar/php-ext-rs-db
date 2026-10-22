//! PostgreSQL driver (`postgres` + `postgres-openssl`).
//!
//! Includes per-connection LRU statement cache, one-shot stale-plan retry,
//! COLLECTION/TDO mapping, LOB limits (via BindSpec), and refcursor FETCH.

use crate::bind_value::{bind_type, map_tdo_to_pg, BindSpec, BindValue, PgCollectionKind};
use crate::config::{GlobalConfig, SessionResetMode};
use crate::dsn::{ColumnCase, Dsn, PgSslMode};
use crate::driver::pg_codec::{array_literal, bind_to_text_param, decode_cell, Raw};
use crate::driver::{ColumnInfo, DbValue, ExecuteOutcome, RowSet};
use crate::error::Error;
use postgres::types::Type;
use postgres::{Client, NoTls, Row, Statement};
use postgres_openssl::MakeTlsConnector;
use std::collections::{HashMap, VecDeque};
use std::time::Duration;

const STMT_SAVEPOINT: &str = "RSDB_STMT";
/// Hard timeout for session reset on release (plan: 5s).
const SESSION_RESET_TIMEOUT_MS: u64 = 5_000;

/// PostgreSQL physical connection wrapper.
pub struct PgConnection {
    client: Client,
    physical_tx: bool,
    statement_rollback: bool,
    stmt_cache: StmtCache,
}

struct StmtCache {
    max: usize,
    order: VecDeque<String>,
    map: HashMap<String, Statement>,
}

impl StmtCache {
    fn new(max: usize) -> Self {
        Self {
            max: max.max(1),
            order: VecDeque::new(),
            map: HashMap::new(),
        }
    }

    fn clear(&mut self) {
        self.order.clear();
        self.map.clear();
    }

    fn remove(&mut self, sql: &str) {
        self.map.remove(sql);
        self.order.retain(|s| s != sql);
    }

    fn get_cloned(&mut self, sql: &str) -> Option<Statement> {
        if self.map.contains_key(sql) {
            if let Some(pos) = self.order.iter().position(|s| s == sql) {
                let key = self.order.remove(pos).unwrap();
                self.order.push_back(key);
            }
            self.map.get(sql).cloned()
        } else {
            None
        }
    }

    fn insert(&mut self, client: &mut Client, sql: String, stmt: Statement) {
        if self.map.contains_key(&sql) {
            self.map.insert(sql.clone(), stmt);
            return;
        }
        while self.map.len() >= self.max {
            if let Some(old) = self.order.pop_front() {
                // Dropping Statement closes/deallocates the server-side handle.
                let _ = self.map.remove(&old);
                let _ = client; // keep signature for future explicit DEALLOCATE
            } else {
                break;
            }
        }
        self.order.push_back(sql.clone());
        self.map.insert(sql, stmt);
    }
}

impl PgConnection {
    pub fn connect(dsn: &Dsn) -> Result<Self, Error> {
        let cfg = GlobalConfig::get();
        let connect_ms = dsn.connect_timeout_ms.unwrap_or(cfg.connect_timeout_ms);
        let call_ms = dsn.call_timeout_ms.unwrap_or(cfg.call_timeout_ms);

        let mut pg = postgres::Config::new();
        pg.host(&dsn.host);
        pg.port(dsn.port);
        pg.user(&dsn.user);
        pg.password(&dsn.password);
        pg.dbname(&dsn.service_or_database);
        pg.connect_timeout(Duration::from_millis(connect_ms.max(1)));

        if let Some(idle) = dsn.keepalive_s {
            pg.keepalives(true);
            pg.keepalives_idle(Duration::from_secs(idle));
        }

        if let Some(ref app) = dsn.application_name {
            pg.application_name(app);
        }

        let mut options = Vec::new();
        if call_ms > 0 {
            options.push(format!("-c statement_timeout={}", call_ms));
        }
        if !options.is_empty() {
            pg.options(&options.join(" "));
        }

        let sslmode = dsn.sslmode.unwrap_or(PgSslMode::Prefer);
        let client = match sslmode {
            PgSslMode::Disable => pg.connect(NoTls).map_err(map_connect_err(dsn))?,
            _ => {
                use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
                let mut builder = SslConnector::builder(SslMethod::tls())
                    .map_err(|e| Error::Connection(e.to_string()))?;
                match sslmode {
                    PgSslMode::Require | PgSslMode::Prefer => {
                        builder.set_verify(SslVerifyMode::NONE);
                    }
                    PgSslMode::VerifyCa | PgSslMode::VerifyFull => {
                        builder.set_verify(SslVerifyMode::PEER);
                        if let Some(ref path) = dsn.sslrootcert {
                            builder
                                .set_ca_file(path)
                                .map_err(|e| Error::Connection(e.to_string()))?;
                        }
                    }
                    PgSslMode::Disable => unreachable!(),
                }
                let tls = MakeTlsConnector::new(builder.build());
                pg.connect(tls).map_err(map_connect_err(dsn))?
            }
        };

        Ok(Self {
            client,
            physical_tx: false,
            statement_rollback: dsn.statement_rollback,
            stmt_cache: StmtCache::new(cfg.stmt_cache_size),
        })
    }

    pub fn ping(&mut self) -> Result<bool, Error> {
        match self.client.simple_query("SELECT 1") {
            Ok(_) => Ok(true),
            Err(e) => {
                let msg = Error::sanitize_message(&e.to_string(), None);
                Err(Error::Connection(msg))
            }
        }
    }

    fn ensure_physical_tx(&mut self) -> Result<(), Error> {
        if !self.physical_tx {
            self.client
                .batch_execute("BEGIN")
                .map_err(map_driver_err)?;
            self.physical_tx = true;
        }
        Ok(())
    }

    pub fn begin(&mut self) -> Result<(), Error> {
        self.ensure_physical_tx()
    }

    pub fn commit(&mut self) -> Result<(), Error> {
        if self.physical_tx {
            self.client.batch_execute("COMMIT").map_err(map_driver_err)?;
            self.physical_tx = false;
        }
        Ok(())
    }

    pub fn rollback(&mut self) -> Result<(), Error> {
        if self.physical_tx {
            self.client
                .batch_execute("ROLLBACK")
                .map_err(map_driver_err)?;
            self.physical_tx = false;
        }
        Ok(())
    }

    pub fn savepoint(&mut self, name: &str) -> Result<(), Error> {
        self.ensure_physical_tx()?;
        let sql = format!("SAVEPOINT {}", quote_ident(name));
        self.client.batch_execute(&sql).map_err(map_driver_err)
    }

    pub fn release_savepoint(&mut self, name: &str) -> Result<(), Error> {
        let sql = format!("RELEASE SAVEPOINT {}", quote_ident(name));
        self.client.batch_execute(&sql).map_err(map_driver_err)
    }

    pub fn rollback_to_savepoint(&mut self, name: &str) -> Result<(), Error> {
        let sql = format!("ROLLBACK TO SAVEPOINT {}", quote_ident(name));
        self.client.batch_execute(&sql).map_err(map_driver_err)?;
        self.release_savepoint(name)
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

    pub fn prepare_execute_specs(
        &mut self,
        sql: &str,
        binds: &[BindSpec],
        column_case: ColumnCase,
    ) -> Result<ExecuteOutcome, Error> {
        self.ensure_physical_tx()?;
        for b in binds {
            b.check_lob_limit()?;
            validate_pg_bind(b)?;
        }

        let run = |c: &mut Self| c.execute_cached(sql, binds, column_case, true);
        if self.statement_rollback && self.physical_tx {
            self.with_statement_savepoint(run)
        } else {
            run(self)
        }
    }

    fn execute_cached(
        &mut self,
        sql: &str,
        binds: &[BindSpec],
        column_case: ColumnCase,
        allow_retry: bool,
    ) -> Result<ExecuteOutcome, Error> {
        let params = build_params(binds)?;
        let param_refs: Vec<&(dyn postgres::types::ToSql + Sync)> =
            params.iter().map(|p| p.as_ref()).collect();

        let stmt = if let Some(s) = self.stmt_cache.get_cloned(sql) {
            s
        } else {
            let s = self.client.prepare(sql).map_err(map_driver_err)?;
            self.stmt_cache
                .insert(&mut self.client, sql.to_string(), s.clone());
            s
        };

        let result = self.run_statement(&stmt, &param_refs, binds, column_case);
        match result {
            Ok(o) => Ok(o),
            Err(e) if allow_retry && is_stale_plan_error(&e) => {
                self.stmt_cache.remove(sql);
                // DEALLOCATE may already have been done by server; ignore.
                let _ = self.client.batch_execute(&format!(
                    "DEALLOCATE ALL"
                ));
                self.stmt_cache.clear();
                self.execute_cached(sql, binds, column_case, false)
            }
            Err(e) => Err(e),
        }
    }

    fn run_statement(
        &mut self,
        stmt: &Statement,
        param_refs: &[&(dyn postgres::types::ToSql + Sync)],
        binds: &[BindSpec],
        column_case: ColumnCase,
    ) -> Result<ExecuteOutcome, Error> {
        let ncol = stmt.columns().len();
        if ncol == 0 {
            let n = self
                .client
                .execute(stmt, param_refs)
                .map_err(map_driver_err)?;
            let out_cursors = self.fetch_out_cursors(binds, column_case)?;
            return Ok(ExecuteOutcome {
                rows_affected: n,
                rowset: None,
                sqlstate: None,
                message: None,
                out_cursors,
                out_binds: Vec::new(),
            });
        }
        let rows = self
            .client
            .query(stmt, param_refs)
            .map_err(map_driver_err)?;
        let rowset = rows_to_set(&rows, stmt.columns(), column_case)?;
        let affected = rows.len() as u64;
        let out_cursors = self.fetch_out_cursors(binds, column_case)?;
        Ok(ExecuteOutcome {
            rows_affected: affected,
            rowset: Some(rowset),
            sqlstate: None,
            message: None,
            out_cursors,
            out_binds: Vec::new(),
        })
    }

    /// After a statement that opened refcursors as OUT params: portal name is in
    /// a text OUT channel. For refcursor OUT we expect the bind value after execute
    /// to be obtained via a follow-up pattern: caller uses `fetch_refcursor`.
    ///
    /// Here we support explicit OUT cursor binds whose IN value is already a portal
    /// name string (set by PL/pgSQL `OPEN`), or CursorPlaceholder meaning the
    /// previous statement returned a single-column refcursor name.
    fn fetch_out_cursors(
        &mut self,
        binds: &[BindSpec],
        column_case: ColumnCase,
    ) -> Result<Vec<(String, RowSet)>, Error> {
        let mut out = Vec::new();
        for b in binds {
            if b.type_name != bind_type::CURSOR && !matches!(b.value, BindValue::CursorPlaceholder)
            {
                continue;
            }
            if !b.is_out && !matches!(b.value, BindValue::CursorPlaceholder) {
                continue;
            }
            let portal = match &b.value {
                BindValue::String(s) if !s.is_empty() => s.clone(),
                BindValue::CursorPlaceholder => continue, // filled by PHP after reading OUT text
                _ => continue,
            };
            let rs = self.fetch_refcursor(&portal, column_case)?;
            out.push((b.name.clone(), rs));
        }
        Ok(out)
    }

    /// `FETCH ALL FROM "<portal>"` in the current transaction.
    pub fn fetch_refcursor(
        &mut self,
        portal_name: &str,
        column_case: ColumnCase,
    ) -> Result<RowSet, Error> {
        self.ensure_physical_tx()?;
        let sql = format!("FETCH ALL FROM {}", quote_ident(portal_name));
        let stmt = self.client.prepare(&sql).map_err(map_driver_err)?;
        let rows = self.client.query(&stmt, &[]).map_err(map_driver_err)?;
        rows_to_set(&rows, stmt.columns(), column_case)
    }

    /// Resolve a portal name string into a RowSet (used by PHP after OUT text bind).
    pub fn materialize_refcursor_name(
        &mut self,
        name: &str,
        column_case: ColumnCase,
    ) -> Result<RowSet, Error> {
        self.fetch_refcursor(name, column_case)
    }

    fn with_statement_savepoint<F>(&mut self, f: F) -> Result<ExecuteOutcome, Error>
    where
        F: FnOnce(&mut Self) -> Result<ExecuteOutcome, Error>,
    {
        self.savepoint(STMT_SAVEPOINT)?;
        match f(self) {
            Ok(v) => {
                self.release_savepoint(STMT_SAVEPOINT)?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.rollback_to_savepoint(STMT_SAVEPOINT);
                Err(e)
            }
        }
    }

    pub fn discard_session(&mut self, mode: SessionResetMode) -> Result<(), Error> {
        // Cap reset to 5s so a hung socket cannot block RSHUTDOWN.
        let _ = self
            .client
            .batch_execute(&format!("SET statement_timeout = {}", SESSION_RESET_TIMEOUT_MS));
        let result = match mode {
            SessionResetMode::Off => Ok(()),
            SessionResetMode::Full => {
                self.physical_tx = false;
                self.stmt_cache.clear();
                self.client
                    .batch_execute("DISCARD ALL")
                    .map_err(map_driver_err)
            }
            SessionResetMode::Soft => {
                self.physical_tx = false;
                // Soft keeps prepared statements; clear only our client-side LRU names
                // that DISCARD would otherwise remove — Soft intentionally skips DEALLOCATE ALL.
                self.client
                    .batch_execute(
                        "RESET ALL; CLOSE ALL; UNLISTEN *; \
                         SELECT pg_advisory_unlock_all(); DISCARD TEMP; DISCARD SEQUENCES",
                    )
                    .map_err(map_driver_err)
            }
        };
        let _ = self.client.batch_execute("RESET statement_timeout");
        result
    }

    pub fn close(self) {
        drop(self.client);
    }

    pub fn apply_call_timeout(&mut self, timeout_ms: u64) -> Result<(), Error> {
        if timeout_ms == 0 {
            self.client
                .batch_execute("RESET statement_timeout")
                .map_err(map_driver_err)
        } else {
            let sql = format!("SET statement_timeout = {}", timeout_ms);
            self.client.batch_execute(&sql).map_err(map_driver_err)
        }
    }
}

fn validate_pg_bind(b: &BindSpec) -> Result<(), Error> {
    match b.type_name.as_str() {
        bind_type::COLLECTION => {
            let tdo = b
                .tdo
                .as_deref()
                .or(match &b.value {
                    BindValue::Collection { tdo, .. } => Some(tdo.as_str()),
                    _ => None,
                })
                .ok_or_else(|| Error::Bind("COLLECTION requires TDO".into()))?;
            let _ = map_tdo_to_pg(tdo)?;
            Ok(())
        }
        bind_type::CURSOR => Ok(()),
        _ => Ok(()),
    }
}

fn build_params(
    binds: &[BindSpec],
) -> Result<Vec<Box<dyn postgres::types::ToSql + Sync>>, Error> {
    let mut out = Vec::with_capacity(binds.len());
    for b in binds {
        // Cursor OUT placeholders are not sent as SQL params when using DO wrappers;
        // for positional $n SQL, skip CursorPlaceholder (should not appear in converted SQL).
        if matches!(b.value, BindValue::CursorPlaceholder) {
            continue;
        }
        let value = match (&b.type_name[..], &b.value, &b.tdo) {
            (bind_type::COLLECTION, BindValue::Array(items), Some(tdo))
            | (bind_type::COLLECTION, BindValue::Collection { items, .. }, Some(tdo)) => {
                let kind = map_tdo_to_pg(tdo)?;
                let coerced = coerce_collection_items(items, kind)?;
                BindValue::Array(coerced)
            }
            (bind_type::COLLECTION, BindValue::Collection { tdo, items }, None) => {
                let kind = map_tdo_to_pg(tdo)?;
                BindValue::Array(coerce_collection_items(items, kind)?)
            }
            (bind_type::CLOB, BindValue::String(s), _) => BindValue::String(s.clone()),
            (bind_type::BLOB, BindValue::Bytes(b), _) => BindValue::Bytes(b.clone()),
            (bind_type::BLOB, BindValue::String(s), _) => BindValue::Bytes(s.as_bytes().to_vec()),
            _ => b.value.clone(),
        };
        out.push(Box::new(bind_to_text_param(&value)) as Box<dyn postgres::types::ToSql + Sync>);
    }
    Ok(out)
}

fn coerce_collection_items(
    items: &[BindValue],
    kind: PgCollectionKind,
) -> Result<Vec<BindValue>, Error> {
    let mut out = Vec::with_capacity(items.len());
    for it in items {
        match (kind, it) {
            (_, BindValue::Null) => out.push(BindValue::Null),
            (PgCollectionKind::Bigint, BindValue::Integer(i)) => out.push(BindValue::Integer(*i)),
            (PgCollectionKind::Bigint, BindValue::String(s)) => {
                let i: i64 = s
                    .parse()
                    .map_err(|_| Error::Bind(format!("D_CL_ID element not integer: {}", s)))?;
                out.push(BindValue::Integer(i));
            }
            (PgCollectionKind::Text, BindValue::String(s)) => out.push(BindValue::String(s.clone())),
            (PgCollectionKind::Text, BindValue::Integer(i)) => {
                out.push(BindValue::String(i.to_string()))
            }
            _ => {
                return Err(Error::Bind(format!(
                    "incompatible collection element for {:?}",
                    kind
                )))
            }
        }
    }
    let _ = array_literal(&out); // ensure encode path works
    Ok(out)
}

fn is_stale_plan_error(e: &Error) -> bool {
    let msg = e.to_string();
    if msg.contains("[26000]") {
        return true;
    }
    if msg.contains("[0A000]")
        && msg
            .to_ascii_lowercase()
            .contains("cached plan must not change result type")
    {
        return true;
    }
    false
}

fn map_connect_err(dsn: &Dsn) -> impl Fn(postgres::Error) -> Error {
    let pwd = dsn.password.clone();
    move |e: postgres::Error| {
        let msg = Error::sanitize_message(&e.to_string(), Some(&pwd));
        Error::Connection(msg)
    }
}

fn map_driver_err(e: postgres::Error) -> Error {
    let code = e.code().map(|c| c.code().to_string());
    let msg = match e.as_db_error() {
        Some(d) => d.message().to_string(),
        None => e.to_string(),
    };
    Error::Driver(format!(
        "{}{}",
        code.map(|c| format!("[{}] ", c)).unwrap_or_default(),
        msg
    ))
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn rows_to_set(
    rows: &[Row],
    columns: &[postgres::Column],
    column_case: ColumnCase,
) -> Result<RowSet, Error> {
    let cols: Vec<ColumnInfo> = columns
        .iter()
        .map(|c| ColumnInfo {
            name: apply_column_case(c.name(), column_case),
        })
        .collect();
    let mut out_rows = Vec::with_capacity(rows.len());
    for row in rows {
        let mut cells = Vec::with_capacity(cols.len());
        for (i, col) in columns.iter().enumerate() {
            cells.push(read_cell(row, i, col)?);
        }
        out_rows.push(cells);
    }
    Ok(RowSet {
        columns: cols,
        rows: out_rows,
    })
}

fn apply_column_case(name: &str, case: ColumnCase) -> String {
    match case {
        ColumnCase::Upper => name.to_ascii_uppercase(),
        ColumnCase::Lower => name.to_ascii_lowercase(),
        ColumnCase::Native => name.to_string(),
    }
}

fn read_cell(row: &Row, idx: usize, col: &postgres::Column) -> Result<DbValue, Error> {
    let _ = Type::TEXT;
    let raw: Option<Raw<'_>> = row
        .try_get(idx)
        .map_err(|e| Error::Driver(format!("column '{}': {}", col.name(), e)))?;
    match raw {
        None => Ok(DbValue::Null),
        Some(Raw(bytes)) => decode_cell(col.name(), col.type_(), bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_plan_detection() {
        let e = Error::Driver(
            "[0A000] cached plan must not change result type".into(),
        );
        assert!(is_stale_plan_error(&e));
        let e2 = Error::Driver("[0A000] something else".into());
        assert!(!is_stale_plan_error(&e2));
        let e3 = Error::Driver("[26000] invalid sql statement name".into());
        assert!(is_stale_plan_error(&e3));
    }
}
