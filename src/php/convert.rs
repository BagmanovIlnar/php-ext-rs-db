//! Conversion helpers between PHP `Zval`s and the Rust core value types,
//! plus the panic guard used by every PHP entry point.
//!
//! Contents:
//!
//! * [`guard`] - `catch_unwind` wrapper turning panics into `PhpException`.
//! * [`db_value_to_zval`], [`bind_value_to_zval`] - Rust → PHP.
//! * [`zval_to_bind_value`] - PHP → [`BindValue`] honouring a declared bind type.
//! * [`decode_out_payload`] - PostgreSQL `current_setting` payload → [`BindValue`].
//! * [`assoc_zval`], [`list_zval`], [`rows_to_zval`] - array builders.
//! * [`current_arg_reference`] - capture a by-reference argument of the running call.
//! * [`write_reference`] - write an OUT value through a captured reference.

use crate::bind_value::{bind_type, BindValue};
use crate::driver::{ColumnInfo, DbValue};
use crate::error::Error;
use ext_php_rs::exception::PhpException;
use ext_php_rs::ffi;
use ext_php_rs::types::{ZendHashTable, ZendStr, Zval};
use ext_php_rs::zend::ExecuteData;
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Result alias used by PHP-facing code.
pub type PhpResult<T> = Result<T, PhpException>;

/// Run `f`, converting a Rust panic into a PHP exception (`RsDataBaseException`).
///
/// Every `#[php_impl]` method body and lifecycle hook goes through this, so a
/// bug in the Rust code can never unwind across the FFI boundary.
///
/// # Example
///
/// ```ignore
/// pub fn ping(&self) -> PhpResult<bool> {
///     guard(|| { /* may panic */ Ok(true) })
/// }
/// ```
pub fn guard<T>(f: impl FnOnce() -> PhpResult<T>) -> PhpResult<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(payload) => {
            let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };
            Err(crate::php::exception::usage_error(format!(
                "Internal error (panic): {}",
                msg
            )))
        }
    }
}

/// PHP string zval (UTF-8 text).
///
/// # Example
///
/// ```ignore
/// let z = str_zval("hello"); // "hello"
/// ```
pub fn str_zval(s: &str) -> Zval {
    let mut z = Zval::new();
    let _ = z.set_string(s, false);
    z
}

/// PHP binary-safe string zval.
///
/// # Example
///
/// ```ignore
/// let z = bytes_zval(&[0xff, 0x00, 0x01]);
/// ```
pub fn bytes_zval(b: &[u8]) -> Zval {
    let mut z = Zval::new();
    z.set_zend_string(ZendStr::new(b, false));
    z
}

/// Build a PHP list (`array`) from zvals.
///
/// # Example
///
/// ```ignore
/// let z = list_zval(vec![str_zval("a"), str_zval("b")]); // ['a', 'b']
/// ```
pub fn list_zval(items: Vec<Zval>) -> Zval {
    let mut ht = ZendHashTable::new();
    for it in items {
        let _ = ht.push(it);
    }
    let mut z = Zval::new();
    z.set_hashtable(ht);
    z
}

/// Build a PHP associative array. On duplicate keys the last value wins.
///
/// # Example
///
/// ```ignore
/// let z = assoc_zval(vec![("id".into(), 1i64.into_zval(false).unwrap())]); // ['id' => 1]
/// ```
pub fn assoc_zval(entries: Vec<(String, Zval)>) -> Zval {
    let mut ht = ZendHashTable::new();
    for (k, v) in entries {
        let _ = ht.insert(k.as_str(), v);
    }
    let mut z = Zval::new();
    z.set_hashtable(ht);
    z
}

/// `i64` → zval.
pub fn long_zval(v: i64) -> Zval {
    let mut z = Zval::new();
    z.set_long(v);
    z
}

/// `bool` → zval.
pub fn bool_zval(v: bool) -> Zval {
    let mut z = Zval::new();
    z.set_bool(v);
    z
}

/// Convert one fetched cell into a PHP value (`NULL`, int, float, bool, string).
///
/// # Example
///
/// ```ignore
/// let z = db_value_to_zval(&DbValue::Integer(42)); // int(42)
/// ```
pub fn db_value_to_zval(v: &DbValue) -> Zval {
    match v {
        DbValue::Null => Zval::null(),
        DbValue::Integer(i) => long_zval(*i),
        DbValue::Float(f) => {
            let mut z = Zval::new();
            z.set_double(*f);
            z
        }
        DbValue::Boolean(b) => bool_zval(*b),
        DbValue::String(s) => str_zval(s),
        DbValue::Bytes(b) => bytes_zval(b),
        DbValue::Array(items) => list_zval(items.iter().map(db_value_to_zval).collect()),
    }
}

/// Convert a [`BindValue`] (e.g. an OUT value) into a PHP value.
///
/// # Example
///
/// ```ignore
/// let z = bind_value_to_zval(&BindValue::Array(vec![BindValue::Integer(1)])); // [1]
/// ```
pub fn bind_value_to_zval(v: &BindValue) -> Zval {
    match v {
        BindValue::Null => Zval::null(),
        BindValue::Integer(i) => long_zval(*i),
        BindValue::Float(f) => {
            let mut z = Zval::new();
            z.set_double(*f);
            z
        }
        BindValue::Boolean(b) => bool_zval(*b),
        BindValue::String(s) => str_zval(s),
        BindValue::Bytes(b) => bytes_zval(b),
        BindValue::Array(items) | BindValue::Collection { items, .. } => {
            list_zval(items.iter().map(bind_value_to_zval).collect())
        }
        BindValue::CursorPlaceholder => Zval::null(),
    }
}

/// Convert one row to an associative array keyed by column name.
///
/// Duplicate column names: the last column wins (like `oci_fetch_assoc`).
///
/// # Example
///
/// ```ignore
/// // columns [ID, NAME], row [1, "x"]  =>  ['ID' => 1, 'NAME' => 'x']
/// let z = row_to_zval(&columns, &row);
/// ```
pub fn row_to_zval(columns: &[ColumnInfo], row: &[DbValue]) -> Zval {
    let entries = columns
        .iter()
        .zip(row.iter())
        .map(|(c, v)| (c.name.clone(), db_value_to_zval(v)))
        .collect();
    assoc_zval(entries)
}

/// Convert a slice of rows into a PHP list of associative arrays.
///
/// # Example
///
/// ```ignore
/// let z = rows_to_zval(&rowset.columns, &rowset.rows[0..2]);
/// ```
pub fn rows_to_zval(columns: &[ColumnInfo], rows: &[Vec<DbValue>]) -> Zval {
    list_zval(rows.iter().map(|r| row_to_zval(columns, r)).collect())
}

/// Infer a [`BindValue`] from a PHP value (`type = null` auto-detection).
///
/// `int→Integer`, `float→Float`, `bool→Boolean`, `string→String`, `null→Null`,
/// `array→Array`.
///
/// # Example
///
/// ```ignore
/// assert_eq!(infer_bind_value(&long_zval(1)).unwrap(), BindValue::Integer(1));
/// ```
pub fn infer_bind_value(zv: &Zval) -> Result<BindValue, Error> {
    let zv = zv.dereference();
    if zv.is_null() {
        Ok(BindValue::Null)
    } else if zv.is_bool() {
        Ok(BindValue::Boolean(zv.bool().unwrap_or(false)))
    } else if zv.is_long() {
        Ok(BindValue::Integer(zv.long().unwrap_or(0)))
    } else if zv.is_double() {
        Ok(BindValue::Float(zv.double().unwrap_or(0.0)))
    } else if zv.is_string() {
        string_bind_value(zv)
    } else if let Some(arr) = zv.array() {
        let mut out = Vec::with_capacity(arr.len());
        for (_, item) in arr.iter() {
            let v = infer_bind_value(item)?;
            if matches!(v, BindValue::Array(_)) {
                return Err(Error::Bind("nested arrays are not supported".into()));
            }
            out.push(v);
        }
        Ok(BindValue::Array(out))
    } else {
        Err(Error::Bind(format!(
            "unsupported PHP value of type {:?}",
            zv.get_type()
        )))
    }
}

fn string_bind_value(zv: &Zval) -> Result<BindValue, Error> {
    let s = zv
        .zend_str()
        .ok_or_else(|| Error::Bind("expected string".into()))?;
    match s.as_str() {
        Ok(text) => Ok(BindValue::String(text.to_string())),
        // Not valid UTF-8: keep the bytes so BLOB-like data is not corrupted.
        Err(_) => Ok(BindValue::Bytes(s.as_bytes().to_vec())),
    }
}

/// Convert a PHP value to a [`BindValue`] honouring the declared bind type.
///
/// `type_name` is one of the `RsDataBasePrepare` constants (`'integer'`, ...)
/// or `None` for auto-detection. A PHP `null` is always SQL `NULL`.
///
/// # Example
///
/// ```ignore
/// let v = zval_to_bind_value(&str_zval("42"), Some("integer"))?; // BindValue::Integer(42)
/// ```
pub fn zval_to_bind_value(zv: &Zval, type_name: Option<&str>) -> Result<BindValue, Error> {
    let zv = zv.dereference();
    let Some(t) = type_name else {
        return infer_bind_value(zv);
    };
    if zv.is_null() {
        return Ok(BindValue::Null);
    }
    match t {
        bind_type::NULL => Ok(BindValue::Null),
        bind_type::INTEGER => zv
            .coerce_to_long()
            .map(BindValue::Integer)
            .ok_or_else(|| Error::Bind("value cannot be converted to integer".into())),
        bind_type::FLOAT => zv
            .coerce_to_double()
            .map(BindValue::Float)
            .ok_or_else(|| Error::Bind("value cannot be converted to float".into())),
        bind_type::BOOLEAN => Ok(BindValue::Boolean(zv.coerce_to_bool())),
        bind_type::STRING | bind_type::CLOB => {
            if zv.is_string() {
                match string_bind_value(zv)? {
                    BindValue::Bytes(_) => Err(Error::Bind(
                        "string value is not valid UTF-8; use the BLOB type".into(),
                    )),
                    other => Ok(other),
                }
            } else {
                zv.coerce_to_string()
                    .map(BindValue::String)
                    .ok_or_else(|| Error::Bind("value cannot be converted to string".into()))
            }
        }
        bind_type::BLOB => {
            if let Some(s) = zv.zend_str() {
                Ok(BindValue::Bytes(s.as_bytes().to_vec()))
            } else {
                zv.coerce_to_string()
                    .map(|s| BindValue::Bytes(s.into_bytes()))
                    .ok_or_else(|| Error::Bind("value cannot be converted to blob".into()))
            }
        }
        bind_type::ARRAY | bind_type::COLLECTION => match infer_bind_value(zv)? {
            v @ BindValue::Array(_) => Ok(v),
            _ => Err(Error::Bind(format!("'{}' bind requires a PHP array", t))),
        },
        bind_type::CURSOR => Ok(BindValue::CursorPlaceholder),
        other => Err(Error::Bind(format!("unknown bind type '{}'", other))),
    }
}

/// Whether `t` is one of the supported bind type names.
///
/// # Example
///
/// ```ignore
/// assert!(is_known_bind_type("integer"));
/// assert!(!is_known_bind_type("int"));
/// ```
pub fn is_known_bind_type(t: &str) -> bool {
    matches!(
        t,
        bind_type::CURSOR
            | bind_type::COLLECTION
            | bind_type::ARRAY
            | bind_type::CLOB
            | bind_type::BLOB
            | bind_type::INTEGER
            | bind_type::FLOAT
            | bind_type::STRING
            | bind_type::BOOLEAN
            | bind_type::NULL
    )
}

/// Decode a PostgreSQL OUT payload (already stripped of the `V` prefix; `None` = SQL NULL)
/// into a [`BindValue`] of the declared bind type.
///
/// # Example
///
/// ```ignore
/// assert_eq!(decode_out_payload(Some("42"), "integer")?, BindValue::Integer(42));
/// assert_eq!(decode_out_payload(None, "string")?, BindValue::Null);
/// assert_eq!(decode_out_payload(Some("{1,2}"), "array")?,
///            BindValue::Array(vec![BindValue::Integer(1), BindValue::Integer(2)]));
/// ```
pub fn decode_out_payload(payload: Option<&str>, type_name: &str) -> Result<BindValue, Error> {
    let Some(text) = payload else {
        return Ok(BindValue::Null);
    };
    match type_name {
        bind_type::INTEGER => text
            .trim()
            .parse::<i64>()
            .map(BindValue::Integer)
            .map_err(|_| Error::Bind(format!("OUT value '{}' is not an integer", text))),
        bind_type::FLOAT => text
            .trim()
            .parse::<f64>()
            .map(BindValue::Float)
            .map_err(|_| Error::Bind(format!("OUT value '{}' is not a float", text))),
        bind_type::BOOLEAN => Ok(BindValue::Boolean(matches!(
            text.trim().to_ascii_lowercase().as_str(),
            "t" | "true" | "1" | "y" | "yes" | "on"
        ))),
        bind_type::BLOB => {
            let hex_part = text.strip_prefix("\\x").unwrap_or(text);
            hex::decode(hex_part)
                .map(BindValue::Bytes)
                .map_err(|_| Error::Bind("OUT value is not a valid bytea".into()))
        }
        bind_type::ARRAY | bind_type::COLLECTION => {
            let items = parse_pg_text_array(text)?;
            let all_int = items
                .iter()
                .all(|i| i.as_ref().map(|s| s.parse::<i64>().is_ok()).unwrap_or(true));
            Ok(BindValue::Array(
                items
                    .into_iter()
                    .map(|i| match i {
                        None => BindValue::Null,
                        Some(s) if all_int => BindValue::Integer(s.parse().unwrap_or(0)),
                        Some(s) => BindValue::String(s),
                    })
                    .collect(),
            ))
        }
        _ => Ok(BindValue::String(text.to_string())),
    }
}

/// Parse a one-dimensional PostgreSQL text array literal (`{a,"b c",NULL}`).
///
/// # Example
///
/// ```ignore
/// let v = parse_pg_text_array(r#"{a,"b,c",NULL}"#)?;
/// assert_eq!(v, vec![Some("a".into()), Some("b,c".into()), None]);
/// ```
pub fn parse_pg_text_array(text: &str) -> Result<Vec<Option<String>>, Error> {
    let t = text.trim();
    let inner = t
        .strip_prefix('{')
        .and_then(|r| r.strip_suffix('}'))
        .ok_or_else(|| Error::Bind("OUT value is not a PostgreSQL array literal".into()))?;
    let mut out = Vec::new();
    let mut chars = inner.chars().peekable();
    if inner.is_empty() {
        return Ok(out);
    }
    loop {
        let mut item = String::new();
        let mut quoted = false;
        if chars.peek() == Some(&'"') {
            quoted = true;
            chars.next();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        if let Some(n) = chars.next() {
                            item.push(n);
                        }
                    }
                    '"' => break,
                    other => item.push(other),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' {
                    break;
                }
                item.push(c);
                chars.next();
            }
        }
        if !quoted && item.trim().eq_ignore_ascii_case("NULL") {
            out.push(None);
        } else {
            out.push(Some(if quoted { item } else { item.trim().to_string() }));
        }
        match chars.next() {
            Some(',') => continue,
            _ => break,
        }
    }
    Ok(out)
}

/// Capture argument `index` (0-based, `$this` excluded) of the *currently executing*
/// internal function as an owning reference zval.
///
/// `ext-php-rs` dereferences by-reference arguments before the Rust body runs, so
/// the `zend_reference` itself is read from the call frame here. The returned zval
/// holds one extra refcount on the reference; dropping it releases that count.
/// Returns `None` if the argument was not passed by reference.
///
/// # Safety
///
/// Must be called synchronously from a `#[php_impl]` method body, on the PHP thread.
///
/// # Example
///
/// ```ignore
/// // inside `bind(&self, name: String, value: PhpRef, ...)`:
/// let reference = unsafe { current_arg_reference(1) }; // `value` is argument #1
/// ```
pub unsafe fn current_arg_reference(index: usize) -> Option<Zval> {
    let eg = ffi::ext_php_rs_executor_globals().as_ref()?;
    let ex: &ExecuteData = eg.current_execute_data.as_ref()?;
    let zv = ex.zend_call_arg(index)?;
    if zv.is_reference() {
        Some(zv.shallow_clone())
    } else {
        None
    }
}

/// Write `value` through the PHP reference held in `reference` (OUT / IN OUT bind).
///
/// Returns `false` (and writes nothing) if `reference` is not a PHP reference.
///
/// # Example
///
/// ```ignore
/// write_reference(&mut bind_ref, str_zval("DOC")); // `$out` now holds "DOC" in PHP
/// ```
pub fn write_reference(reference: &mut Zval, value: Zval) -> bool {
    match reference.reference_mut() {
        Some(inner) => {
            // Assignment drops the previous value (zval_ptr_dtor) and moves the new one in.
            *inner = value;
            true
        }
        None => false,
    }
}