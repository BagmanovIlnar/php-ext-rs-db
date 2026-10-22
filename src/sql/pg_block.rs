//! PostgreSQL anonymous block wrapper (IN/OUT via transactional `set_config`).

use crate::bind_value::BindValue;
use crate::sql::tokenizer::ParsedSql;
use std::fmt::Write as _;

/// 16 hex chars identifying one block execution.
pub fn new_execution_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:016x}", nanos as u64)
}

/// Config key `rsdb.b{execution_id}.p{index}` (index is placeholder pg_index - 1 or 0-based bind order).
pub fn bind_config_key(execution_id: &str, bind_index: usize) -> String {
    format!("rsdb.b{}.p{}", execution_id, bind_index)
}

/// `N` = SQL NULL, `V<text>` = value (distinguishes NULL from empty string).
pub fn encode_nv(value: &BindValue) -> String {
    crate::bind_value::encode_set_config_payload(value)
}

/// Decode `current_setting` payload back to optional string body (without V prefix).
pub fn decode_nv(payload: &str) -> Option<String> {
    if payload.is_empty() {
        return None;
    }
    if payload.starts_with('N') {
        None
    } else if let Some(rest) = payload.strip_prefix('V') {
        Some(rest.to_string())
    } else {
        Some(payload.to_string())
    }
}

/// Build `SELECT set_config($1,$2,true), ...` pairs for IN binds.
pub fn build_set_config_batch(keys: &[String], values: &[BindValue]) -> (String, Vec<String>) {
    debug_assert_eq!(keys.len(), values.len());
    if keys.is_empty() {
        return ("SELECT 1".to_string(), Vec::new());
    }
    let mut params = Vec::with_capacity(keys.len() * 2);
    let mut sql = String::from("SELECT ");
    for (i, (k, v)) in keys.iter().zip(values.iter()).enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        let p_key = params.len() + 1;
        params.push(k.clone());
        let p_val = params.len() + 1;
        params.push(encode_nv(v));
        write!(sql, "set_config(${}, ${}, true)", p_key, p_val).unwrap();
    }
    (sql, params)
}

/// PL/pgSQL type for a bind variable from type name (plan section 7).
pub fn pg_decl_type(type_name: Option<&str>) -> &'static str {
    match type_name {
        Some("integer") => "bigint",
        Some("float") => "double precision",
        Some("boolean") => "boolean",
        Some("blob") => "bytea",
        Some("array") | Some("collection") => "text[]",
        // OUT refcursor portal name is text; FETCH happens after execute.
        Some("cursor") => "text",
        _ => "text",
    }
}

/// Dollar tag for DO wrapper; must not appear in user SQL.
pub fn dollar_tag(execution_id: &str) -> String {
    format!("rsdb_{}", execution_id)
}

/// Generate full `DO $tag$ ... $tag$` wrapper for a user block.
pub fn build_do_wrapper(
    execution_id: &str,
    user_block_sql: &str,
    bind_count: usize,
    bind_types: &[Option<String>],
    out_indices: &[usize],
) -> String {
    let tag = dollar_tag(execution_id);
    let body = ParsedSql::replace_bare_return_for_pg_wrap(user_block_sql);
    let mut sql = format!("DO ${tag}$\nDECLARE\n");
    for i in 0..bind_count {
        let ty = pg_decl_type(bind_types.get(i).and_then(|t| t.as_deref()));
        if i > 0 {
            sql.push_str("  ");
        }
        writeln!(sql, "rsdb_v{i} {ty};").unwrap();
    }
    sql.push_str("BEGIN\n");
    for i in 0..bind_count {
        let key = bind_config_key(execution_id, i);
        let ty = pg_decl_type(bind_types.get(i).and_then(|t| t.as_deref()));
        writeln!(
            sql,
            "  rsdb_v{i} := CASE WHEN left(current_setting('{key}', true), 1) = 'N' THEN NULL \
             ELSE substr(current_setting('{key}', true), 2)::{ty} END;"
        )
        .unwrap();
    }
    sql.push_str("  <<rsdb_main>>\n  BEGIN\n    ");
    sql.push_str(body.trim());
    if !body.trim_end().ends_with(';') {
        sql.push(';');
    }
    sql.push_str("\n  END rsdb_main;\n");
    for &i in out_indices {
        let key = bind_config_key(execution_id, i);
        writeln!(
            sql,
            "  PERFORM set_config('{key}', CASE WHEN rsdb_v{i} IS NULL THEN 'N' \
             ELSE 'V' || rsdb_v{i}::text END, true);"
        )
        .unwrap();
    }
    writeln!(sql, "END ${tag}$;").unwrap();
    sql
}

/// Read OUT values: `SELECT current_setting($1,true), ...`
pub fn build_read_out_settings(keys: &[String]) -> (String, Vec<String>) {
    if keys.is_empty() {
        return ("SELECT 1".to_string(), Vec::new());
    }
    let mut sql = String::from("SELECT ");
    for (i, _) in keys.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        write!(sql, "current_setting(${}, true)", i + 1).unwrap();
    }
    (sql, keys.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_id_is_16_hex() {
        let id = new_execution_id();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn bind_key_format() {
        assert_eq!(bind_config_key("abcd", 0), "rsdb.babcd.p0");
    }

    #[test]
    fn nv_roundtrip() {
        assert_eq!(decode_nv("N"), None);
        assert_eq!(decode_nv("Vhello"), Some("hello".into()));
    }

    #[test]
    fn do_wrapper_contains_tag_and_main() {
        let w = build_do_wrapper(
            "0011223344556677",
            "BEGIN NULL; END;",
            1,
            &[Some("integer".into())],
            &[0],
        );
        assert!(w.contains("$rsdb_0011223344556677$"));
        assert!(w.contains("<<rsdb_main>>"));
        assert!(w.contains("rsdb_v0"));
    }

    #[test]
    fn set_config_batch_empty() {
        let (sql, p) = build_set_config_batch(&[], &[]);
        assert_eq!(sql, "SELECT 1");
        assert!(p.is_empty());
    }
}
