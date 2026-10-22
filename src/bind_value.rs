//! Owned bind values (no PHP `Zval`) and driver-facing bind specs.

use crate::config::GlobalConfig;
use crate::error::Error;

/// Bind type string constants (match `RsDataBasePrepare` plan).
pub mod bind_type {
    pub const CURSOR: &str = "cursor";
    pub const COLLECTION: &str = "collection";
    pub const ARRAY: &str = "array";
    pub const CLOB: &str = "clob";
    pub const BLOB: &str = "blob";
    pub const INTEGER: &str = "integer";
    pub const FLOAT: &str = "float";
    pub const STRING: &str = "string";
    pub const BOOLEAN: &str = "boolean";
    pub const NULL: &str = "null";
}

/// Rust-owned value for a bind parameter.
#[derive(Debug, Clone, PartialEq)]
pub enum BindValue {
    Null,
    Integer(i64),
    Float(f64),
    Boolean(bool),
    String(String),
    Bytes(Vec<u8>),
    Array(Vec<BindValue>),
    /// OUT REF CURSOR / refcursor placeholder (no IN payload).
    CursorPlaceholder,
    /// Typed collection: Oracle object type name / PG array via TDO map.
    Collection {
        tdo: String,
        items: Vec<BindValue>,
    },
}

impl BindValue {
    /// Infer bind type name when PHP type is unknown.
    pub fn inferred_type_name(&self) -> &'static str {
        match self {
            BindValue::Null => bind_type::NULL,
            BindValue::Integer(_) => bind_type::INTEGER,
            BindValue::Float(_) => bind_type::FLOAT,
            BindValue::Boolean(_) => bind_type::BOOLEAN,
            BindValue::String(_) => bind_type::STRING,
            BindValue::Bytes(_) => bind_type::BLOB,
            BindValue::Array(_) => bind_type::ARRAY,
            BindValue::CursorPlaceholder => bind_type::CURSOR,
            BindValue::Collection { .. } => bind_type::COLLECTION,
        }
    }

    /// Whether the value is SQL NULL.
    pub fn is_null(&self) -> bool {
        matches!(self, BindValue::Null)
    }

    /// Byte length for LOB limit checks (`String` / `Bytes` / nested).
    pub fn payload_bytes(&self) -> usize {
        match self {
            BindValue::String(s) => s.len(),
            BindValue::Bytes(b) => b.len(),
            BindValue::Array(items) | BindValue::Collection { items, .. } => {
                items.iter().map(|i| i.payload_bytes()).sum()
            }
            _ => 0,
        }
    }
}

/// Driver-facing bind descriptor (data only, no PHP refs).
#[derive(Debug, Clone, PartialEq)]
pub struct BindSpec {
    pub name: String,
    pub value: BindValue,
    pub type_name: String,
    pub tdo: Option<String>,
    pub is_out: bool,
}

impl BindSpec {
    pub fn new(
        name: impl Into<String>,
        value: BindValue,
        type_name: impl Into<String>,
        tdo: Option<String>,
        is_out: bool,
    ) -> Self {
        Self {
            name: name.into(),
            value,
            type_name: type_name.into(),
            tdo,
            is_out,
        }
    }

    /// Enforce `lob_max_bytes` for CLOB/BLOB/string/bytes payloads.
    pub fn check_lob_limit(&self) -> Result<(), Error> {
        let is_lob = matches!(
            self.type_name.as_str(),
            bind_type::CLOB | bind_type::BLOB
        ) || matches!(self.value, BindValue::Bytes(_));
        if !is_lob {
            return Ok(());
        }
        let max = GlobalConfig::get().lob_max_bytes;
        let n = self.value.payload_bytes();
        if n > max {
            return Err(Error::Bind(format!(
                "LOB/bind payload {} bytes exceeds lob_max_bytes={}",
                n, max
            )));
        }
        Ok(())
    }
}

/// Map Oracle/legacy TDO names to PostgreSQL array element kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgCollectionKind {
    Bigint,
    Text,
}

/// Resolve TDO → PostgreSQL array type. Unknown TDO is an error.
pub fn map_tdo_to_pg(tdo: &str) -> Result<PgCollectionKind, Error> {
    match tdo.trim().to_ascii_uppercase().as_str() {
        "D_CL_ID" => Ok(PgCollectionKind::Bigint),
        "D_CL_STR" => Ok(PgCollectionKind::Text),
        other if other.is_empty() => Err(Error::Bind(
            "COLLECTION bind requires setTdo() (e.g. D_CL_ID, D_CL_STR)".into(),
        )),
        other => Err(Error::Bind(format!(
            "unknown COLLECTION TDO '{}'; supported: D_CL_ID, D_CL_STR",
            other
        ))),
    }
}

/// Encode for PostgreSQL transactional `set_config` (plan section 7).
pub fn encode_set_config_payload(value: &BindValue) -> String {
    match value {
        BindValue::Null | BindValue::CursorPlaceholder => "N".to_string(),
        BindValue::Integer(v) => format!("V{}", v),
        BindValue::Float(v) => format!("V{}", v),
        BindValue::Boolean(v) => format!("V{}", v),
        BindValue::String(s) => format!("V{}", s),
        BindValue::Bytes(b) => format!("V\\x{}", hex::encode(b)),
        BindValue::Array(items) | BindValue::Collection { items, .. } => {
            let inner: Vec<String> = items
                .iter()
                .map(|v| match v {
                    BindValue::Null => "NULL".to_string(),
                    BindValue::String(s) => pg_array_elem(s),
                    BindValue::Integer(i) => i.to_string(),
                    BindValue::Float(f) => f.to_string(),
                    BindValue::Boolean(b) => b.to_string(),
                    BindValue::Bytes(b) => format!("\\x{}", hex::encode(b)),
                    BindValue::Array(_)
                    | BindValue::Collection { .. }
                    | BindValue::CursorPlaceholder => "NULL".to_string(),
                })
                .collect();
            format!("V{{{}}}", inner.join(","))
        }
    }
}

fn pg_array_elem(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\"", escaped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_encoding() {
        assert_eq!(encode_set_config_payload(&BindValue::Null), "N");
    }

    #[test]
    fn string_encoding() {
        assert_eq!(
            encode_set_config_payload(&BindValue::String("x".into())),
            "Vx"
        );
    }

    #[test]
    fn tdo_map() {
        assert_eq!(map_tdo_to_pg("D_CL_ID").unwrap(), PgCollectionKind::Bigint);
        assert_eq!(map_tdo_to_pg("d_cl_str").unwrap(), PgCollectionKind::Text);
        assert!(map_tdo_to_pg("UNKNOWN").is_err());
    }
}
