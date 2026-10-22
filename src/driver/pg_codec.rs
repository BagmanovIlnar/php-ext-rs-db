//! PostgreSQL wire codec helpers for the `postgres` driver.
//!
//! * [`TextParam`] - sends any bind as a **text-format** parameter, so the server parses
//!   it according to the parameter type it inferred (`int4`, `numeric`, `date`, `uuid`,
//!   `jsonb`, arrays, ...). This avoids "cannot convert between Rust type i64 and int4"
//!   errors and gives a typed `NULL` for every parameter type.
//! * [`Raw`] + [`decode_cell`] - read a result cell from its binary representation by type.
//!
//! Result decoding (binary format, which `rust-postgres` always requests):
//!
//! | PostgreSQL                               | [`DbValue`]                         |
//! | ---------------------------------------- | ----------------------------------- |
//! | `int2/int4/int8/oid`                     | `Integer`                           |
//! | `float4/float8`                          | `Float`                             |
//! | `numeric`                                | `String` (no precision loss)        |
//! | `bool`                                   | `Boolean`                           |
//! | `text/varchar/bpchar/name/char`          | `String`                            |
//! | `bytea`                                  | `Bytes`                             |
//! | `uuid`, `json`, `jsonb`                  | `String`                            |
//! | `date`, `timestamp`                      | `String` `Y-m-d[ H:i:s[.u]]`        |
//! | `timestamptz`                            | `String` ISO-8601 with `+00:00`     |
//! | `time`                                   | `String` `H:i:s[.u]`                |
//! | one-dimensional arrays of the above      | `Array`                             |
//! | anything else                            | error naming the column and OID     |

use crate::bind_value::BindValue;
use crate::driver::DbValue;
use crate::error::Error;
use postgres::types::private::BytesMut;
use postgres::types::{to_sql_checked, FromSql, IsNull, ToSql, Type};
use std::error::Error as StdError;
use std::fmt::Write as _;

/// A bind parameter sent in text format (`None` = SQL `NULL`).
///
/// # Example
///
/// ```ignore
/// let p = bind_to_text_param(&BindValue::Integer(42)); // "42", parsed by the server as int4/int8/numeric...
/// ```
#[derive(Debug)]
pub struct TextParam(pub Option<String>);

impl ToSql for TextParam {
    fn to_sql(&self, _ty: &Type, out: &mut BytesMut) -> Result<IsNull, Box<dyn StdError + Sync + Send>> {
        match &self.0 {
            None => Ok(IsNull::Yes),
            Some(s) => {
                out.extend_from_slice(s.as_bytes());
                Ok(IsNull::No)
            }
        }
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    fn encode_format(&self, _ty: &Type) -> postgres::types::Format {
        postgres::types::Format::Text
    }

    to_sql_checked!();
}

/// Convert a [`BindValue`] to its PostgreSQL text input representation.
///
/// # Example
///
/// ```ignore
/// assert_eq!(bind_to_text_param(&BindValue::Boolean(true)).0.as_deref(), Some("true"));
/// assert_eq!(bind_to_text_param(&BindValue::Null).0, None);
/// ```
pub fn bind_to_text_param(v: &BindValue) -> TextParam {
    TextParam(match v {
        BindValue::Null | BindValue::CursorPlaceholder => None,
        BindValue::Integer(i) => Some(i.to_string()),
        BindValue::Float(f) => Some(f.to_string()),
        BindValue::Boolean(b) => Some(if *b { "true" } else { "false" }.to_string()),
        BindValue::String(s) => Some(s.clone()),
        BindValue::Bytes(b) => Some(format!("\\x{}", hex::encode(b))),
        BindValue::Array(items) | BindValue::Collection { items, .. } => {
            Some(array_literal(items))
        }
    })
}

/// PostgreSQL array literal (`{"a","b",NULL}`) with correct escaping.
///
/// # Example
///
/// ```ignore
/// let s = array_literal(&[BindValue::String("a\"b".into()), BindValue::Null]);
/// assert_eq!(s, r#"{"a\"b",NULL}"#);
/// ```
pub fn array_literal(items: &[BindValue]) -> String {
    let mut out = String::from("{");
    for (i, it) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match it {
            BindValue::Null => out.push_str("NULL"),
            BindValue::Integer(v) => out.push_str(&v.to_string()),
            BindValue::Float(v) => out.push_str(&v.to_string()),
            BindValue::Boolean(v) => out.push_str(if *v { "true" } else { "false" }),
            BindValue::String(s) => quote_array_elem(&mut out, s),
            BindValue::Bytes(b) => quote_array_elem(&mut out, &format!("\\x{}", hex::encode(b))),
            BindValue::Array(_)
            | BindValue::Collection { .. }
            | BindValue::CursorPlaceholder => out.push_str("NULL"),
        }
    }
    out.push('}');
    out
}

fn quote_array_elem(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
}

/// Raw bytes of a binary-format result cell (accepts every type).
pub struct Raw<'a>(pub &'a [u8]);

impl<'a> FromSql<'a> for Raw<'a> {
    fn from_sql(_ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn StdError + Sync + Send>> {
        Ok(Raw(raw))
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }
}

fn bad(col: &str, ty: &Type, why: &str) -> Error {
    Error::Driver(format!(
        "cannot decode column '{}' of type '{}' (oid {}): {}",
        col,
        ty.name(),
        ty.oid(),
        why
    ))
}

/// Decode one non-NULL binary cell of type `ty`. `col` is only used in error messages.
///
/// # Example
///
/// ```ignore
/// let v = decode_cell("id", &Type::INT8, &42i64.to_be_bytes())?; // DbValue::Integer(42)
/// ```
pub fn decode_cell(col: &str, ty: &Type, raw: &[u8]) -> Result<DbValue, Error> {
    if let postgres::types::Kind::Array(elem) = ty.kind() {
        return decode_array(col, ty, elem, raw);
    }
    decode_scalar(col, ty, raw)
}

fn be_i16(col: &str, ty: &Type, raw: &[u8]) -> Result<i16, Error> {
    raw.try_into().map(i16::from_be_bytes).map_err(|_| bad(col, ty, "bad length"))
}
fn be_i32(col: &str, ty: &Type, raw: &[u8]) -> Result<i32, Error> {
    raw.try_into().map(i32::from_be_bytes).map_err(|_| bad(col, ty, "bad length"))
}
fn be_i64(col: &str, ty: &Type, raw: &[u8]) -> Result<i64, Error> {
    raw.try_into().map(i64::from_be_bytes).map_err(|_| bad(col, ty, "bad length"))
}

fn utf8(col: &str, ty: &Type, raw: &[u8]) -> Result<String, Error> {
    std::str::from_utf8(raw)
        .map(|s| s.to_string())
        .map_err(|_| bad(col, ty, "invalid UTF-8"))
}

fn decode_scalar(col: &str, ty: &Type, raw: &[u8]) -> Result<DbValue, Error> {
    let t = ty;
    if *t == Type::BOOL {
        Ok(DbValue::Boolean(raw.first().copied().unwrap_or(0) != 0))
    } else if *t == Type::INT2 {
        Ok(DbValue::Integer(i64::from(be_i16(col, t, raw)?)))
    } else if *t == Type::INT4 {
        Ok(DbValue::Integer(i64::from(be_i32(col, t, raw)?)))
    } else if *t == Type::INT8 {
        Ok(DbValue::Integer(be_i64(col, t, raw)?))
    } else if *t == Type::OID {
        Ok(DbValue::Integer(i64::from(be_i32(col, t, raw)? as u32)))
    } else if *t == Type::FLOAT4 {
        let bits = be_i32(col, t, raw)? as u32;
        Ok(DbValue::Float(f64::from(f32::from_bits(bits))))
    } else if *t == Type::FLOAT8 {
        Ok(DbValue::Float(f64::from_bits(be_i64(col, t, raw)? as u64)))
    } else if *t == Type::NUMERIC {
        Ok(DbValue::String(decode_numeric(col, t, raw)?))
    } else if *t == Type::TEXT
        || *t == Type::VARCHAR
        || *t == Type::BPCHAR
        || *t == Type::NAME
        || *t == Type::UNKNOWN
        || *t == Type::JSON
    {
        Ok(DbValue::String(utf8(col, t, raw)?))
    } else if *t == Type::CHAR {
        Ok(DbValue::String(
            raw.first().map(|b| (*b as char).to_string()).unwrap_or_default(),
        ))
    } else if *t == Type::JSONB {
        match raw.split_first() {
            Some((1, rest)) => Ok(DbValue::String(utf8(col, t, rest)?)),
            _ => Err(bad(col, t, "unsupported jsonb version")),
        }
    } else if *t == Type::BYTEA {
        Ok(DbValue::Bytes(raw.to_vec()))
    } else if *t == Type::UUID {
        if raw.len() != 16 {
            return Err(bad(col, t, "bad length"));
        }
        let h = hex::encode(raw);
        Ok(DbValue::String(format!(
            "{}-{}-{}-{}-{}",
            &h[0..8],
            &h[8..12],
            &h[12..16],
            &h[16..20],
            &h[20..32]
        )))
    } else if *t == Type::DATE {
        let days = be_i32(col, t, raw)?;
        Ok(DbValue::String(match days {
            i32::MAX => "infinity".to_string(),
            i32::MIN => "-infinity".to_string(),
            d => format_date(i64::from(d)),
        }))
    } else if *t == Type::TIMESTAMP || *t == Type::TIMESTAMPTZ {
        let us = be_i64(col, t, raw)?;
        if us == i64::MAX {
            return Ok(DbValue::String("infinity".into()));
        }
        if us == i64::MIN {
            return Ok(DbValue::String("-infinity".into()));
        }
        let days = us.div_euclid(86_400_000_000);
        let rem = us.rem_euclid(86_400_000_000);
        let mut s = format!("{} {}", format_date(days), format_time(rem));
        if *t == Type::TIMESTAMPTZ {
            s = s.replacen(' ', "T", 1);
            s.push_str("+00:00");
        }
        Ok(DbValue::String(s))
    } else if *t == Type::TIME {
        Ok(DbValue::String(format_time(be_i64(col, t, raw)?)))
    } else {
        Err(bad(
            col,
            t,
            "unsupported type; cast it in SQL (e.g. `col::text`)",
        ))
    }
}

/// Days since 2000-01-01 → `YYYY-MM-DD` (proleptic Gregorian).
fn format_date(days_since_2000: i64) -> String {
    // Howard Hinnant's civil_from_days, shifted from 1970 to 2000 (10957 days).
    let z = days_since_2000 + 10_957 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// Microseconds since midnight → `HH:MM:SS[.uuuuuu]`.
fn format_time(us: i64) -> String {
    let secs = us / 1_000_000;
    let frac = us % 1_000_000;
    let mut s = format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60);
    if frac != 0 {
        let _ = write!(s, ".{:06}", frac);
    }
    s
}

/// Binary `numeric` → decimal string without precision loss.
fn decode_numeric(col: &str, ty: &Type, raw: &[u8]) -> Result<String, Error> {
    if raw.len() < 8 {
        return Err(bad(col, ty, "bad numeric length"));
    }
    let ndigits = i16::from_be_bytes([raw[0], raw[1]]) as usize;
    let weight = i16::from_be_bytes([raw[2], raw[3]]) as i32;
    let sign = u16::from_be_bytes([raw[4], raw[5]]);
    let dscale = u16::from_be_bytes([raw[6], raw[7]]) as usize;
    if sign == 0xC000 {
        return Ok("NaN".into());
    }
    if raw.len() < 8 + ndigits * 2 {
        return Err(bad(col, ty, "truncated numeric"));
    }
    let digits: Vec<i32> = (0..ndigits)
        .map(|i| i32::from(i16::from_be_bytes([raw[8 + i * 2], raw[9 + i * 2]])))
        .collect();

    let mut int_part = String::new();
    let mut idx = 0usize;
    if weight < 0 {
        int_part.push('0');
    } else {
        for w in (0..=weight).rev() {
            let _ = w;
            let d = digits.get(idx).copied().unwrap_or(0);
            idx += 1;
            if int_part.is_empty() {
                let _ = write!(int_part, "{}", d);
            } else {
                let _ = write!(int_part, "{:04}", d);
            }
        }
    }
    let mut frac = String::new();
    if dscale > 0 {
        // Leading zero groups when weight < -1.
        let mut w = weight + 1;
        while w < 0 {
            frac.push_str("0000");
            w += 1;
        }
        while idx < digits.len() {
            let _ = write!(frac, "{:04}", digits[idx]);
            idx += 1;
        }
        while frac.len() < dscale {
            frac.push('0');
        }
        frac.truncate(dscale);
    }
    let mut s = String::new();
    if sign == 0x4000 {
        s.push('-');
    }
    s.push_str(&int_part);
    if dscale > 0 {
        s.push('.');
        s.push_str(&frac);
    }
    Ok(s)
}

fn decode_array(col: &str, ty: &Type, elem: &Type, raw: &[u8]) -> Result<DbValue, Error> {
    if raw.len() < 12 {
        return Err(bad(col, ty, "bad array header"));
    }
    let ndim = be_i32(col, ty, &raw[0..4])?;
    if ndim == 0 {
        return Ok(DbValue::Array(Vec::new()));
    }
    if ndim != 1 {
        return Err(bad(col, ty, "multi-dimensional arrays are not supported"));
    }
    if raw.len() < 20 {
        return Err(bad(col, ty, "bad array header"));
    }
    let len = be_i32(col, ty, &raw[12..16])?.max(0) as usize;
    let mut pos = 20usize;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        let l = be_i32(col, ty, raw.get(pos..pos + 4).ok_or_else(|| bad(col, ty, "truncated array"))?)?;
        pos += 4;
        if l < 0 {
            out.push(DbValue::Null);
        } else {
            let l = l as usize;
            let cell = raw.get(pos..pos + l).ok_or_else(|| bad(col, ty, "truncated array"))?;
            pos += l;
            out.push(decode_scalar(col, elem, cell)?);
        }
    }
    Ok(DbValue::Array(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_params() {
        assert_eq!(bind_to_text_param(&BindValue::Integer(-5)).0.as_deref(), Some("-5"));
        assert_eq!(bind_to_text_param(&BindValue::Null).0, None);
        assert_eq!(
            bind_to_text_param(&BindValue::Bytes(vec![0xde, 0xad])).0.as_deref(),
            Some("\\xdead")
        );
    }

    #[test]
    fn array_literals_escape() {
        let s = array_literal(&[
            BindValue::String("a\"b".into()),
            BindValue::Null,
            BindValue::Integer(3),
        ]);
        assert_eq!(s, "{\"a\\\"b\",NULL,3}");
    }

    #[test]
    fn dates() {
        assert_eq!(format_date(0), "2000-01-01");
        assert_eq!(format_date(366), "2001-01-01");
        assert_eq!(format_date(-1), "1999-12-31");
        assert_eq!(format_time(3_661_000_500), "01:01:01.000500");
    }

    #[test]
    fn numeric_decoding() {
        // 12345.678 : ndigits=3 weight=1 sign=0 dscale=3 digits [1, 2345, 6780]
        let mut raw = Vec::new();
        for v in [3i16, 1, 0, 3, 1, 2345, 6780] {
            raw.extend_from_slice(&v.to_be_bytes());
        }
        assert_eq!(decode_numeric("n", &Type::NUMERIC, &raw).unwrap(), "12345.678");
        // 0.05 : ndigits=1 weight=-1 dscale=2 digits [500]
        let mut raw = Vec::new();
        for v in [1i16, -1, 0, 2, 500] {
            raw.extend_from_slice(&v.to_be_bytes());
        }
        assert_eq!(decode_numeric("n", &Type::NUMERIC, &raw).unwrap(), "0.05");
    }

    #[test]
    fn scalars() {
        assert_eq!(
            decode_cell("c", &Type::INT4, &7i32.to_be_bytes()).unwrap(),
            DbValue::Integer(7)
        );
        assert_eq!(
            decode_cell("c", &Type::BOOL, &[1]).unwrap(),
            DbValue::Boolean(true)
        );
    }
}
