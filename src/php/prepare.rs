//! `RsDataBasePrepare` - a prepared SQL statement: binds, execute, fetch.
//!
//! Flow of `execute()`:
//!
//! 1. validate the lease (closed / stale / broken; optional reconnect);
//! 2. resolve binds against the placeholders of the cached [`ParsedSql`]
//!    (`sql::parse_cached` ran once in `RsDataBaseConnect::prepare()`; rejected SQL
//!    never becomes a statement);
//! 3. convert PHP values to owned [`BindValue`]s **before** touching the driver;
//! 4. run by query kind:
//!    * `select` / `dml` - `$1..$N` SQL straight to the driver;
//!    * `returning_into` (PostgreSQL) - `INTO` stripped, first row becomes OUT values;
//!    * `block` (PostgreSQL) - generated `DO` wrapper from [`crate::sql::pg_block`]:
//!      `set_config` IN values → `DO` → read OUT values, all inside one savepoint;
//! 5. only after complete success OUT values are written into the PHP references.
//!
//! # Example (PHP)
//!
//! ```php
//! $q = $db->prepare('SELECT id, name FROM organisation WHERE active = :active');
//! $q->bindValue('active', true, -1, RsDataBasePrepare::BOOLEAN);
//! if (!$q->execute()) { throw new RsDataBaseException($q->getErrorMessage()); }
//! foreach ($q->fetchAllRows() as $row) { echo $row['NAME']; }
//! ```

use crate::bind_value::{bind_type, BindSpec, BindValue};
use crate::driver::{DatabaseConnection, DbValue, ExecuteOutcome, RowSet};
use crate::dsn::{ColumnCase, DriverKind};
use crate::error::Error;
use crate::php::bind::{normalize_bind_name, BindState, RsDataBaseBind};
use crate::php::connect::{
    is_connection_lost, note_failure, reconnect_state, run_on_connection, stale_lease_error,
};
use crate::php::convert::{
    assoc_zval, bind_value_to_zval, bool_zval, current_arg_reference, decode_out_payload, guard,
    is_known_bind_type, list_zval, long_zval, str_zval, write_reference, zval_to_bind_value,
    PhpResult,
};
use crate::php::cursor::{RowCursor, RsDataBaseCursor};
use crate::php::exception::{connection_error, error_parts, usage_error};
use crate::php::request::{self, StateRef};
use crate::sql::pg_block;
use crate::sql::{self, ParsedSql, Placeholder, QueryKind};
use ext_php_rs::convert::IntoZval;
use ext_php_rs::flags::ClassFlags;
use ext_php_rs::prelude::*;
use ext_php_rs::types::{PhpRef, ZendClassObject, Zval};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

/// Savepoint wrapping a PostgreSQL anonymous block / `RETURNING INTO` execution.
const BLOCK_SAVEPOINT: &str = "RSDB_BLK";

struct BindEntry {
    /// PHP object (`RsDataBaseBind`), returned by `getBinds()`.
    obj: Zval,
    state: Rc<RefCell<BindState>>,
}

struct LogEntry {
    sql: String,
    binds: usize,
    duration_ms: f64,
    success: bool,
    rows: u64,
    error_code: Option<String>,
    error_message: Option<String>,
}

/// One placeholder resolved to its bind and converted value.
struct Resolved {
    name: String,
    value: BindValue,
    type_name: String,
    tdo: Option<String>,
    by_ref: bool,
    state: Rc<RefCell<BindState>>,
}

impl Resolved {
    fn to_spec(&self) -> BindSpec {
        let mut value = self.value.clone();
        if self.type_name == bind_type::CURSOR {
            value = BindValue::CursorPlaceholder;
        } else if self.type_name == bind_type::COLLECTION {
            if let BindValue::Array(items) = value {
                value = BindValue::Collection {
                    tdo: self.tdo.clone().unwrap_or_default(),
                    items,
                };
            }
        }
        BindSpec::new(
            self.name.clone(),
            value,
            self.type_name.clone(),
            self.tdo.clone(),
            self.by_ref,
        )
    }
}

/// What `run()` produced: the driver outcome plus OUT values `(index into resolved, value)`.
struct RunOutput {
    outcome: ExecuteOutcome,
    out_values: Vec<(usize, BindValue)>,
    /// `false` for kinds whose rowset is internal (blocks, RETURNING INTO).
    expose_rows: bool,
}

/// PHP class `RsDataBasePrepare` (final). Created only by `RsDataBaseConnect::prepare()`.
#[php_class]
#[php(name = "RsDataBasePrepare")]
#[php(flags = ClassFlags::Final)]
pub struct RsDataBasePrepare {
    conn_obj: Option<Zval>,
    state: StateRef,
    epoch: u64,
    parsed: ParsedSql,
    binds: Vec<BindEntry>,
    cursor: RowCursor,
    executed: bool,
    affected: u64,
    freed: bool,
    error_code: Option<String>,
    error_message: Option<String>,
    log_enabled: bool,
    log: Vec<LogEntry>,
}

impl RsDataBasePrepare {
    /// Build a statement for `sql` on the connection `conn_obj` / `state`.
    ///
    /// Parses through the global cache; rejected SQL (`BEGIN;`, `COMMIT`, `COMMIT` inside a
    /// block, ...) fails here with `RsDataBaseException` before any DB access.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let p = RsDataBasePrepare::create(conn_obj_zval, state, "SELECT 1".into())?;
    /// ```
    pub fn create(conn_obj: Zval, state: StateRef, sql_text: String) -> PhpResult<Self> {
        if sql_text.trim().is_empty() {
            return Err(usage_error("SQL must not be empty"));
        }
        let (driver, epoch) = {
            let s = state.borrow();
            (s.driver, s.epoch)
        };
        let parsed = sql::parse_cached(driver, &sql_text);
        if parsed.kind == QueryKind::Rejected {
            return Err(usage_error(
                parsed
                    .rejection_reason
                    .clone()
                    .unwrap_or_else(|| "SQL statement is rejected".to_string()),
            ));
        }
        Ok(Self {
            conn_obj: Some(conn_obj),
            state,
            epoch,
            parsed,
            binds: Vec::new(),
            cursor: RowCursor::default(),
            executed: false,
            affected: 0,
            freed: false,
            error_code: None,
            error_message: None,
            log_enabled: false,
            log: Vec::new(),
        })
    }

    fn check_not_freed(&self) -> PhpResult<()> {
        if self.freed {
            Err(usage_error("statement was freed"))
        } else {
            Ok(())
        }
    }

    /// Statement is usable: not freed, lease open, same epoch.
    fn check_lease(&self) -> PhpResult<()> {
        self.check_not_freed()?;
        let s = self.state.borrow();
        if s.closed || s.lease.is_none() || s.epoch != self.epoch {
            return Err(stale_lease_error());
        }
        Ok(())
    }

    fn check_fetch(&self) -> PhpResult<()> {
        self.check_lease()?;
        if !self.executed {
            return Err(usage_error(
                "execute() must be called successfully before fetching rows",
            ));
        }
        Ok(())
    }

    /// Append a bind (validating duplicate names) and return its PHP object (owning zval).
    fn add_bind(&mut self, st: BindState) -> PhpResult<Zval> {
        if let Some(name) = st.name.as_deref() {
            let dup = self.binds.iter().any(|b| {
                let s = b.state.borrow();
                s.attached && s.name.as_deref() == Some(name)
            });
            if dup {
                return Err(usage_error(format!("bind ':{}' already exists", name)));
            }
        }
        let state = Rc::new(RefCell::new(st));
        request::track_bind(&state);
        let obj = ZendClassObject::new(RsDataBaseBind::from_state(state.clone()));
        let zv = obj
            .into_zval(false)
            .map_err(|e| usage_error(format!("cannot create RsDataBaseBind: {:?}", e)))?;
        self.binds.push(BindEntry {
            obj: zv.shallow_clone(),
            state,
        });
        Ok(zv)
    }

    /// Validate / reconnect the lease before executing.
    fn prepare_lease(&mut self, reconnect: bool) -> PhpResult<()> {
        self.check_not_freed()?;
        {
            let s = self.state.borrow();
            if s.closed || s.lease.is_none() || s.epoch != self.epoch {
                return Err(stale_lease_error());
            }
            if s.broken {
                return Err(connection_error(
                    "connection is broken; call rollBack() or close()",
                ));
            }
            if reconnect && (s.physical_tx_active || s.tx_level > 0) {
                return Err(usage_error(
                    "execute(reconnect): reconnect is not allowed while a transaction is active",
                ));
            }
        }
        if reconnect {
            let alive = run_on_connection(&self.state, |c| c.ping()).unwrap_or(false);
            if !alive {
                reconnect_state(&self.state)?;
                self.epoch = self.state.borrow().epoch;
            }
        }
        Ok(())
    }

    /// Match placeholders with attached binds and convert the values.
    fn resolve_binds(&self) -> PhpResult<Vec<Resolved>> {
        let mut names: Vec<String> = Vec::new();
        for ph in &self.parsed.placeholders {
            if ph.pg_index == names.len() + 1 {
                names.push(ph.name.clone());
            }
        }

        let attached: Vec<&BindEntry> = self
            .binds
            .iter()
            .filter(|b| b.state.borrow().attached)
            .collect();
        for b in &attached {
            if b.state.borrow().name.is_none() {
                return Err(usage_error("a bind has no name; call setName()"));
            }
        }

        let mut out = Vec::with_capacity(names.len());
        for name in &names {
            let matches: Vec<&&BindEntry> = attached
                .iter()
                .filter(|b| b.state.borrow().name.as_deref() == Some(name.as_str()))
                .collect();
            let entry = match matches.len() {
                0 => return Err(usage_error(format!("placeholder ':{}' has no bind", name))),
                1 => matches[0],
                _ => return Err(usage_error(format!("duplicate bind ':{}'", name))),
            };
            let st = entry.state.borrow();
            let declared = st.type_name.clone();
            let tdo = st.tdo.clone();
            if declared.as_deref() == Some(bind_type::COLLECTION) && tdo.as_deref().unwrap_or("").is_empty()
            {
                return Err(usage_error(
                    "COLLECTION bind requires setTdo() (e.g. D_CL_ID, D_CL_STR)",
                ));
            }
            let value = if declared.as_deref() == Some(bind_type::CURSOR) {
                BindValue::CursorPlaceholder
            } else {
                match st.value.as_ref() {
                    Some(z) => zval_to_bind_value(z, declared.as_deref())?,
                    None => BindValue::Null,
                }
            };
            let type_name = declared.unwrap_or_else(|| value.inferred_type_name().to_string());
            out.push(Resolved {
                name: name.clone(),
                value,
                type_name,
                tdo,
                by_ref: st.by_ref,
                state: entry.state.clone(),
            });
        }

        for b in &attached {
            let st = b.state.borrow();
            let n = st.name.as_deref().unwrap_or("");
            if !names.iter().any(|x| x == n) {
                return Err(usage_error(format!("bind ':{}' is not used in the SQL", n)));
            }
        }
        Ok(out)
    }

    /// Execute according to the query kind. Pure Rust, no PHP values touched.
    fn run(&self, resolved: &[Resolved]) -> Result<RunOutput, Error> {
        let (driver, column_case) = {
            let s = self.state.borrow();
            (s.driver, s.column_case)
        };
        let parsed = &self.parsed;
        match parsed.kind {
            QueryKind::Rejected => Err(Error::Sql(
                parsed
                    .rejection_reason
                    .clone()
                    .unwrap_or_else(|| "SQL statement is rejected".into()),
            )),
            QueryKind::Select | QueryKind::Dml => {
                let specs: Vec<BindSpec> = resolved.iter().map(|r| r.to_spec()).collect();
                let sql_text = parsed.converted_sql.clone();
                let outcome = run_on_connection(&self.state, |c| {
                    c.prepare_execute_specs(&sql_text, &specs, column_case)
                })?;
                Ok(RunOutput {
                    outcome,
                    out_values: Vec::new(),
                    expose_rows: true,
                })
            }
            QueryKind::ReturningInto if driver == DriverKind::Pg => {
                self.run_returning_into(resolved, column_case)
            }
            QueryKind::Block if driver == DriverKind::Pg => {
                self.run_pg_block(resolved, column_case)
            }
            // Oracle (or any driver executing the statement natively).
            QueryKind::ReturningInto | QueryKind::Block => {
                let specs: Vec<BindSpec> = resolved.iter().map(|r| r.to_spec()).collect();
                let sql_text = parsed.converted_sql.clone();
                let outcome = run_on_connection(&self.state, |c| {
                    c.prepare_execute_specs(&sql_text, &specs, column_case)
                })?;
                let mut out_values = Vec::new();
                for (name, val) in &outcome.out_binds {
                    if let Some(idx) = resolved.iter().position(|r| &r.name == name) {
                        out_values.push((idx, val.clone()));
                    }
                }
                Ok(RunOutput {
                    outcome,
                    out_values,
                    expose_rows: false,
                })
            }
        }
    }

    /// PostgreSQL `INSERT/UPDATE/DELETE ... RETURNING ... INTO :a, :b`.
    ///
    /// `INTO` is already stripped from `converted_sql`; the first returned row is mapped
    /// to the INTO binds (zero rows → NULLs, more than one → error with rollback).
    fn run_returning_into(
        &self,
        resolved: &[Resolved],
        column_case: ColumnCase,
    ) -> Result<RunOutput, Error> {
        let into = &self.parsed.returning_into_names;
        // Parameters of the stripped SQL: every bind that is not an INTO target.
        let params: Vec<BindValue> = resolved
            .iter()
            .filter(|r| !into.contains(&r.name))
            .map(|r| r.value.clone())
            .collect();
        let sql_text = self.parsed.converted_sql.clone();
        let outcome = run_on_connection(&self.state, |c| {
            c.savepoint(BLOCK_SAVEPOINT)?;
            let res = c.prepare_execute(&sql_text, &params, column_case).and_then(|o| {
                let n = o.rowset.as_ref().map(|r| r.rows.len()).unwrap_or(0);
                if n > 1 {
                    Err(Error::Sql(
                        "RETURNING ... INTO matched more than one row".into(),
                    ))
                } else {
                    Ok(o)
                }
            });
            match res {
                Ok(o) => {
                    c.release_savepoint(BLOCK_SAVEPOINT)?;
                    Ok(o)
                }
                Err(e) => {
                    let _ = c.rollback_to_savepoint(BLOCK_SAVEPOINT);
                    Err(e)
                }
            }
        })?;

        let row: Option<&Vec<DbValue>> = outcome.rowset.as_ref().and_then(|r| r.rows.first());
        let mut out_values = Vec::new();
        for (i, name) in into.iter().enumerate() {
            if let Some(idx) = resolved.iter().position(|r| &r.name == name) {
                let cell = row.and_then(|r| r.get(i)).unwrap_or(&DbValue::Null);
                out_values.push((idx, db_to_bind_value(cell, &resolved[idx].type_name)));
            }
        }
        Ok(RunOutput {
            outcome,
            out_values,
            expose_rows: false,
        })
    }

    /// PostgreSQL anonymous block via the `DO` wrapper (IN/OUT through transactional `set_config`).
    fn run_pg_block(
        &self,
        resolved: &[Resolved],
        column_case: ColumnCase,
    ) -> Result<RunOutput, Error> {
        let parsed = &self.parsed;
        let exec_id = pg_block::new_execution_id();
        let tag = pg_block::dollar_tag(&exec_id);
        if parsed.normalized_sql.contains(&tag) {
            return Err(Error::Internal(
                "generated dollar tag collides with the SQL text; retry".into(),
            ));
        }
        let block_sql = substitute_block_placeholders(&parsed.normalized_sql, &parsed.placeholders);
        let types: Vec<Option<String>> = resolved.iter().map(|r| Some(r.type_name.clone())).collect();
        let out_indices: Vec<usize> = resolved
            .iter()
            .enumerate()
            .filter(|(_, r)| r.by_ref)
            .map(|(i, _)| i)
            .collect();
        let keys: Vec<String> = (0..resolved.len())
            .map(|i| pg_block::bind_config_key(&exec_id, i))
            .collect();
        let values: Vec<BindValue> = resolved.iter().map(|r| r.value.clone()).collect();
        let do_sql =
            pg_block::build_do_wrapper(&exec_id, &block_sql, resolved.len(), &types, &out_indices);
        let out_keys: Vec<String> = out_indices.iter().map(|&i| keys[i].clone()).collect();

        let payloads: Vec<Option<String>> = run_on_connection(&self.state, |c| {
            c.savepoint(BLOCK_SAVEPOINT)?;
            let res = exec_pg_block_steps(c, &keys, &values, &do_sql, &out_keys, column_case);
            match res {
                Ok(v) => {
                    c.release_savepoint(BLOCK_SAVEPOINT)?;
                    Ok(v)
                }
                Err(e) => {
                    let _ = c.rollback_to_savepoint(BLOCK_SAVEPOINT);
                    Err(e)
                }
            }
        })?;

        let mut out_values = Vec::new();
        for (k, &idx) in out_indices.iter().enumerate() {
            let payload = payloads.get(k).and_then(|p| p.as_deref());
            out_values.push((idx, decode_out_payload(payload, &resolved[idx].type_name)?));
        }
        Ok(RunOutput {
            outcome: ExecuteOutcome::empty_ok(0),
            out_values,
            expose_rows: false,
        })
    }

    /// Write `RsDataBaseCursor` objects into OUT CURSOR bind references.
    fn apply_out_cursors(&self, resolved: &[Resolved], run: &RunOutput) -> PhpResult<()> {
        let column_case = self.state.borrow().column_case;
        let driver = self.state.borrow().driver;

        // Oracle / driver-provided cursor rowsets.
        for (name, rowset) in &run.outcome.out_cursors {
            if let Some(r) = resolved.iter().find(|r| &r.name == name) {
                if r.by_ref {
                    let cursor =
                        RsDataBaseCursor::new_from_rowset(self.state.clone(), rowset.clone());
                    let z = ZendClassObject::new(cursor)
                        .into_zval(false)
                        .map_err(|e| usage_error(format!("cursor object: {:?}", e)))?;
                    if let Some(dst) = r.state.borrow_mut().value.as_mut() {
                        write_reference(dst, z);
                    }
                }
            }
        }

        // PostgreSQL: CURSOR OUT may arrive as portal name string in out_values.
        if driver == DriverKind::Pg {
            for (idx, val) in &run.out_values {
                let r = &resolved[*idx];
                if r.type_name != bind_type::CURSOR {
                    continue;
                }
                let portal = match val {
                    BindValue::String(s) if !s.is_empty() => s.clone(),
                    _ => continue,
                };
                let rowset = run_on_connection(&self.state, |c| {
                    c.fetch_refcursor(&portal, column_case)
                })?;
                if r.by_ref {
                    let cursor = RsDataBaseCursor::new_from_rowset(self.state.clone(), rowset);
                    let z = ZendClassObject::new(cursor)
                        .into_zval(false)
                        .map_err(|e| usage_error(format!("cursor object: {:?}", e)))?;
                    if let Some(dst) = r.state.borrow_mut().value.as_mut() {
                        write_reference(dst, z);
                    }
                }
            }
        }
        Ok(())
    }

    fn execute_impl(&mut self, reconnect: bool) -> PhpResult<bool> {
        self.prepare_lease(reconnect)?;
        self.cursor.clear();
        self.executed = false;
        self.affected = 0;
        self.error_code = None;
        self.error_message = None;

        let resolved = self.resolve_binds()?;
        let started = Instant::now();
        let result = self.run(&resolved);
        let duration_ms = started.elapsed().as_secs_f64() * 1000.0;
        let sql_for_log = self.parsed.converted_sql.clone();
        let bind_count = resolved.len();

        match result {
            Ok(run) => {
                self.executed = true;
                self.affected = run.outcome.rows_affected;
                if run.expose_rows {
                    self.cursor.set(run.outcome.rowset.clone());
                }
                // OUT scalar values (non-cursor) after complete success.
                for (idx, val) in &run.out_values {
                    let r = &resolved[*idx];
                    if r.type_name == bind_type::CURSOR {
                        continue;
                    }
                    if r.by_ref {
                        if let Some(z) = r.state.borrow_mut().value.as_mut() {
                            write_reference(z, bind_value_to_zval(val));
                        }
                    }
                }
                self.apply_out_cursors(&resolved, &run)?;
                {
                    let mut s = self.state.borrow_mut();
                    s.physical_tx_active = true;
                    s.clear_error();
                }
                if self.log_enabled {
                    self.log.push(LogEntry {
                        sql: sql_for_log,
                        binds: bind_count,
                        duration_ms,
                        success: true,
                        rows: self.affected,
                        error_code: None,
                        error_message: None,
                    });
                }
                Ok(true)
            }
            Err(e) => {
                let lost = is_connection_lost(&e);
                let e = note_failure(&self.state, e);
                let (code, msg) = error_parts(&e);
                self.error_code = code.clone();
                self.error_message = Some(msg.clone());
                if self.log_enabled {
                    self.log.push(LogEntry {
                        sql: sql_for_log,
                        binds: bind_count,
                        duration_ms,
                        success: false,
                        rows: 0,
                        error_code: code,
                        error_message: Some(msg),
                    });
                }
                if lost {
                    return Err(e.into());
                }
                let mut s = self.state.borrow_mut();
                s.physical_tx_active = true;
                let plain = matches!(self.parsed.kind, QueryKind::Select | QueryKind::Dml);
                if plain
                    && s.driver == DriverKind::Pg
                    && !s.statement_rollback
                    && matches!(e, Error::Driver(_))
                {
                    s.tx_failed = true;
                }
                Ok(false)
            }
        }
    }
}

/// The three round-trips of a PostgreSQL block, inside the caller's savepoint:
/// `set_config` (IN values) → `DO` → read OUT settings. Returns OUT payloads
/// (`None` = SQL NULL) in `out_keys` order.
fn exec_pg_block_steps(
    c: &mut DatabaseConnection,
    keys: &[String],
    values: &[BindValue],
    do_sql: &str,
    out_keys: &[String],
    column_case: ColumnCase,
) -> Result<Vec<Option<String>>, Error> {
    if !keys.is_empty() {
        let (sql_text, params) = pg_block::build_set_config_batch(keys, values);
        let params: Vec<BindValue> = params.into_iter().map(BindValue::String).collect();
        c.prepare_execute(&sql_text, &params, column_case)?;
    }
    c.prepare_execute(do_sql, &[], column_case)?;
    if out_keys.is_empty() {
        return Ok(Vec::new());
    }
    let (sql_text, params) = pg_block::build_read_out_settings(out_keys);
    let params: Vec<BindValue> = params.into_iter().map(BindValue::String).collect();
    let outcome = c.prepare_execute(&sql_text, &params, column_case)?;
    let row = outcome
        .rowset
        .and_then(|rs: RowSet| rs.rows.into_iter().next())
        .ok_or_else(|| Error::Internal("OUT settings query returned no row".into()))?;
    Ok(row
        .into_iter()
        .map(|cell| match cell {
            DbValue::String(s) => pg_block::decode_nv(&s),
            _ => None,
        })
        .collect())
}

/// Replace `:name` placeholders of a block with the PL/pgSQL variables `rsdb_v{index}`.
///
/// # Example
///
/// ```ignore
/// // "BEGIN :a := :b; END;"  =>  "BEGIN rsdb_v0 := rsdb_v1; END;"
/// ```
fn substitute_block_placeholders(sql_text: &str, placeholders: &[Placeholder]) -> String {
    let mut out = String::with_capacity(sql_text.len() + placeholders.len() * 4);
    let mut last = 0;
    for ph in placeholders {
        out.push_str(&sql_text[last..ph.start]);
        out.push_str(&format!("rsdb_v{}", ph.pg_index - 1));
        last = ph.end;
    }
    out.push_str(&sql_text[last..]);
    out
}

/// Convert a driver cell to a [`BindValue`] honouring the declared OUT type.
fn db_to_bind_value(v: &DbValue, type_name: &str) -> BindValue {
    match (v, type_name) {
        (DbValue::Null, _) => BindValue::Null,
        (DbValue::String(s), bind_type::INTEGER) => s
            .trim()
            .parse::<i64>()
            .map(BindValue::Integer)
            .unwrap_or_else(|_| BindValue::String(s.clone())),
        (DbValue::String(s), bind_type::FLOAT) => s
            .trim()
            .parse::<f64>()
            .map(BindValue::Float)
            .unwrap_or_else(|_| BindValue::String(s.clone())),
        (DbValue::Integer(i), _) => BindValue::Integer(*i),
        (DbValue::Float(f), _) => BindValue::Float(*f),
        (DbValue::Boolean(b), _) => BindValue::Boolean(*b),
        (DbValue::String(s), _) => BindValue::String(s.clone()),
        (DbValue::Bytes(b), _) => BindValue::Bytes(b.clone()),
        (DbValue::Array(a), _) => {
            BindValue::Array(a.iter().map(|x| db_to_bind_value(x, type_name)).collect())
        }
    }
}

#[php_impl]
impl RsDataBasePrepare {
    /// Bind type: Oracle `REF CURSOR` OUT (value becomes `RsDataBaseCursor`).
    pub const CURSOR: &'static str = "cursor";
    /// Bind type: collection (Oracle TDO / PostgreSQL array).
    pub const COLLECTION: &'static str = "collection";
    /// Bind type: array.
    pub const ARRAY: &'static str = "array";
    /// Bind type: character LOB.
    pub const CLOB: &'static str = "clob";
    /// Bind type: binary LOB / `bytea`.
    pub const BLOB: &'static str = "blob";
    /// Bind type: integer.
    pub const INTEGER: &'static str = "integer";
    /// Bind type: float.
    pub const FLOAT: &'static str = "float";
    /// Bind type: string.
    pub const STRING: &'static str = "string";
    /// Bind type: boolean.
    pub const BOOLEAN: &'static str = "boolean";
    /// Bind type: SQL NULL.
    pub const NULL: &'static str = "null";

    /// Private constructor: statements come from `RsDataBaseConnect::prepare()`.
    ///
    /// ```php
    /// new RsDataBasePrepare(); // Error: Call to private ...
    /// ```
    #[php(vis = "private")]
    pub fn __construct() -> PhpResult<Self> {
        Err(usage_error(
            "RsDataBasePrepare can only be created by RsDataBaseConnect::prepare()",
        ))
    }

    /// Create an empty, attached bind; configure it with the fluent setters.
    ///
    /// ```php
    /// $id = 42;
    /// $q->createBind()->setName('id')->setValue($id)->setType(RsDataBasePrepare::INTEGER);
    /// ```
    pub fn create_bind(&mut self) -> PhpResult<Zval> {
        guard(|| {
            self.check_not_freed()?;
            self.add_bind(BindState::new())
        })
    }

    /// Bind a PHP variable by reference (IN / IN OUT). `id` and `:id` are equivalent.
    ///
    /// ```php
    /// $out = null;
    /// $q->bind('result', $out, 4000, RsDataBasePrepare::STRING);
    /// ```
    #[php(optional = size)]
    pub fn bind<'a>(
        self_: &'a mut ZendClassObject<RsDataBasePrepare>,
        variable_name: String,
        value: PhpRef<'_>,
        size: Option<i64>,
        r#type: Option<String>,
        tdo: Option<String>,
    ) -> PhpResult<&'a mut ZendClassObject<RsDataBasePrepare>> {
        let _ = &value;
        guard(|| {
            self_.check_not_freed()?;
            let name = normalize_bind_name(&variable_name)?;
            check_type(r#type.as_deref())?;
            // SAFETY: synchronous call from this method body; `value` is argument #1.
            let reference = unsafe { current_arg_reference(1) }.ok_or_else(|| {
                usage_error("RsDataBasePrepare::bind() requires a variable passed by reference")
            })?;
            let mut st = BindState::new();
            st.name = Some(name);
            st.value = Some(reference);
            st.by_ref = true;
            st.size = size.unwrap_or(-1);
            st.type_name = r#type;
            st.tdo = tdo;
            self_.add_bind(st)?;
            Ok(())
        })?;
        Ok(self_)
    }

    /// Bind an owned copy of a value (IN only).
    ///
    /// ```php
    /// $q->bindValue('active', true, -1, RsDataBasePrepare::BOOLEAN);
    /// ```
    #[php(optional = size)]
    pub fn bind_value<'a>(
        self_: &'a mut ZendClassObject<RsDataBasePrepare>,
        variable_name: String,
        value: &Zval,
        size: Option<i64>,
        r#type: Option<String>,
        tdo: Option<String>,
    ) -> PhpResult<&'a mut ZendClassObject<RsDataBasePrepare>> {
        guard(|| {
            self_.check_not_freed()?;
            let name = normalize_bind_name(&variable_name)?;
            check_type(r#type.as_deref())?;
            let mut st = BindState::new();
            st.name = Some(name);
            st.value = Some(value.dereference().shallow_clone());
            st.by_ref = false;
            st.size = size.unwrap_or(-1);
            st.type_name = r#type;
            st.tdo = tdo;
            self_.add_bind(st)?;
            Ok(())
        })?;
        Ok(self_)
    }

    /// Detach a bind by name (`'id'` / `':id'`) or by `RsDataBaseBind` object.
    /// Returns `false` if no such bind is attached.
    ///
    /// ```php
    /// $q->removeBind(':id');
    /// $q->removeBind($bindObject);
    /// ```
    pub fn remove_bind(&mut self, bind: &Zval) -> PhpResult<bool> {
        guard(|| {
            self.check_not_freed()?;
            let bind = bind.dereference();
            let pos = if bind.is_string() {
                let name = normalize_bind_name(bind.str().unwrap_or(""))?;
                self.binds.iter().position(|b| {
                    let s = b.state.borrow();
                    s.attached && s.name.as_deref() == Some(name.as_str())
                })
            } else if let Some(obj) = bind.object() {
                let id = obj.get_id();
                self.binds
                    .iter()
                    .position(|b| b.obj.object().map(|o| o.get_id()) == Some(id))
            } else {
                return Err(usage_error(
                    "removeBind() expects a bind name or an RsDataBaseBind object",
                ));
            };
            match pos {
                Some(i) => {
                    let entry = self.binds.remove(i);
                    entry.state.borrow_mut().invalidate();
                    Ok(true)
                }
                None => Ok(false),
            }
        })
    }

    /// Attached binds in creation order.
    ///
    /// ```php
    /// foreach ($q->getBinds() as $bind) { /* RsDataBaseBind */ }
    /// ```
    pub fn get_binds(&self) -> PhpResult<Zval> {
        guard(|| {
            Ok(list_zval(
                self.binds
                    .iter()
                    .filter(|b| b.state.borrow().attached)
                    .map(|b| b.obj.shallow_clone())
                    .collect(),
            ))
        })
    }

    /// The SQL text given to `prepare()`; `null` after `free()`.
    ///
    /// ```php
    /// echo $q->getSql();
    /// ```
    pub fn get_sql(&self) -> Option<String> {
        if self.freed {
            None
        } else {
            Some(self.parsed.original_sql.clone())
        }
    }

    /// `true` for anonymous blocks (`BEGIN ...` / `DECLARE ...`).
    ///
    /// ```php
    /// var_dump($db->prepare('BEGIN :x := 1; END;')->isPLSQL()); // true
    /// ```
    #[php(name = "isPLSQL")]
    pub fn is_plsql(&self) -> bool {
        self.parsed.is_plsql
    }

    /// Execute with the current bind values. Returns `true` on success, `false` on a driver
    /// error (see `getErrorCode()` / `getErrorMessage()`). Usage errors and a closed/broken
    /// connection throw. `execute(true)` may reconnect only when no transaction is active.
    ///
    /// ```php
    /// if (!$q->execute()) { $db->rollBack(); throw new RsDataBaseException($q->getErrorMessage()); }
    /// ```
    #[php(defaults(reconnect = false))]
    pub fn execute(&mut self, reconnect: bool) -> PhpResult<bool> {
        guard(|| self.execute_impl(reconnect))
    }

    /// Next row as an associative array (keys follow `column_case`), `null` after the last row.
    ///
    /// ```php
    /// while (($row = $q->fetchRow()) !== null) { echo $row['ID']; }
    /// ```
    pub fn fetch_row(&mut self) -> PhpResult<Option<Zval>> {
        guard(|| {
            self.check_fetch()?;
            Ok(self.cursor.fetch_row())
        })
    }

    /// Up to `$count` next rows (batch fetch).
    ///
    /// ```php
    /// while ($rows = $q->fetchRows(500)) { process($rows); }
    /// ```
    pub fn fetch_rows(&mut self, count: i64) -> PhpResult<Zval> {
        guard(|| {
            self.check_fetch()?;
            if count < 0 {
                return Err(usage_error("fetchRows(): count must be >= 0"));
            }
            let z = self.cursor.fetch_rows(count as usize);
            Ok(z)
        })
    }

    /// All remaining rows in memory.
    ///
    /// ```php
    /// $rows = $q->fetchAllRows();
    /// ```
    pub fn fetch_all_rows(&mut self) -> PhpResult<Zval> {
        guard(|| {
            self.check_fetch()?;
            Ok(self.cursor.fetch_all())
        })
    }

    /// Rows affected (DML) or returned (SELECT) by the last `execute()`.
    ///
    /// ```php
    /// echo $q->getAffectedRows();
    /// ```
    pub fn get_affected_rows(&self) -> i64 {
        self.affected as i64
    }

    /// Number of result columns (0 when the statement returned no rows).
    ///
    /// ```php
    /// echo $q->getColumnCount();
    /// ```
    pub fn get_column_count(&self) -> i64 {
        self.cursor.column_count() as i64
    }

    /// The `RsDataBaseConnect` this statement belongs to (the same PHP object).
    ///
    /// ```php
    /// var_dump($q->getConnection() === $db); // true
    /// ```
    pub fn get_connection(&self) -> PhpResult<Zval> {
        guard(|| match self.conn_obj.as_ref() {
            Some(z) => Ok(z.shallow_clone()),
            None => Err(usage_error("statement was freed")),
        })
    }

    /// Release the result, the binds and the connection reference (not the connection itself).
    ///
    /// ```php
    /// $q->free();
    /// ```
    pub fn free(&mut self) -> PhpResult<()> {
        guard(|| {
            self.freed = true;
            self.cursor.clear();
            self.executed = false;
            for b in self.binds.drain(..) {
                b.state.borrow_mut().invalidate();
            }
            self.conn_obj = None;
            Ok(())
        })
    }

    /// Last error code (SQLSTATE / ORA-xxxxx) of this statement, or `null`.
    ///
    /// ```php
    /// echo $q->getErrorCode(); // 23505
    /// ```
    pub fn get_error_code(&self) -> Option<String> {
        self.error_code.clone()
    }

    /// Last error message of this statement, or `null`.
    ///
    /// ```php
    /// echo $q->getErrorMessage();
    /// ```
    pub fn get_error_message(&self) -> Option<String> {
        self.error_message.clone()
    }

    /// Start recording executions (SQL, bind count, duration, result; never bind values).
    ///
    /// ```php
    /// $q->enableLog();
    /// ```
    pub fn enable_log(&mut self) {
        self.log_enabled = true;
    }

    /// Stop recording (the collected log is kept).
    ///
    /// ```php
    /// $q->disableLog();
    /// ```
    pub fn disable_log(&mut self) {
        self.log_enabled = false;
    }

    /// Recorded executions: `sql`, `binds`, `duration_ms`, `success`, `rows`, `error_code`, `error_message`.
    ///
    /// ```php
    /// print_r($q->getLog());
    /// ```
    pub fn get_log(&self) -> PhpResult<Zval> {
        guard(|| {
            Ok(list_zval(
                self.log
                    .iter()
                    .map(|l| {
                        let mut duration = Zval::new();
                        duration.set_double(l.duration_ms);
                        assoc_zval(vec![
                            ("sql".into(), str_zval(&l.sql)),
                            ("binds".into(), long_zval(l.binds as i64)),
                            ("duration_ms".into(), duration),
                            ("success".into(), bool_zval(l.success)),
                            ("rows".into(), long_zval(l.rows as i64)),
                            (
                                "error_code".into(),
                                l.error_code.as_deref().map(str_zval).unwrap_or_else(Zval::null),
                            ),
                            (
                                "error_message".into(),
                                l.error_message
                                    .as_deref()
                                    .map(str_zval)
                                    .unwrap_or_else(Zval::null),
                            ),
                        ])
                    })
                    .collect(),
            ))
        })
    }
}

fn check_type(t: Option<&str>) -> PhpResult<()> {
    match t {
        Some(t) if !is_known_bind_type(t) => Err(usage_error(format!("unknown bind type '{}'", t))),
        _ => Ok(()),
    }
}
