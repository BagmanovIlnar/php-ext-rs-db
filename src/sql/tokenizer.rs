//! SQL lexer and query classification for bind placeholders.

use crate::dsn::DriverKind;
use std::collections::HashMap;

/// Classified query kind after lexing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    Select,
    Dml,
    Block,
    ReturningInto,
    Rejected,
}

/// Named bind placeholder (`:name`) location in the original SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placeholder {
    pub name: String,
    pub start: usize,
    pub end: usize,
    /// 1-based index for PostgreSQL `$n`.
    pub pg_index: usize,
}

/// A segment of the original SQL: literal text or placeholder slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlSegment {
    Literal { start: usize, end: usize },
    Placeholder { pg_index: usize },
}

/// Immutable parse result (safe to cache in `Arc`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSql {
    pub original_sql: String,
    pub normalized_sql: String,
    pub segments: Vec<SqlSegment>,
    pub placeholders: Vec<Placeholder>,
    pub kind: QueryKind,
    pub is_plsql: bool,
    pub converted_sql: String,
    pub rejection_reason: Option<String>,
    /// Bind names from `RETURNING ... INTO :a, :b` (without leading `:`).
    pub returning_into_names: Vec<String>,
}

impl ParsedSql {
    /// Parse and classify SQL for the given driver.
    pub fn parse(driver: DriverKind, sql: &str) -> Self {
        let normalized = normalize_sql(sql);
        let mut placeholders = find_placeholders(&normalized);
        assign_pg_indices(&mut placeholders);

        let mut rejection = None;
        let is_plsql = detect_is_plsql(&normalized, &mut rejection);
        let kind = if rejection.is_some() {
            QueryKind::Rejected
        } else {
            classify_kind(&normalized, is_plsql, &placeholders, &mut rejection)
        };

        if kind == QueryKind::Block && rejection.is_none() {
            if let Some(msg) = find_commit_rollback_in_block(&normalized) {
                rejection = Some(msg);
            }
        }

        let kind = if rejection.is_some() {
            QueryKind::Rejected
        } else {
            kind
        };

        let returning_into_names = if kind == QueryKind::ReturningInto {
            extract_returning_into_names(&normalized)
        } else {
            Vec::new()
        };

        let converted_sql = if kind == QueryKind::Rejected {
            normalized.clone()
        } else {
            build_converted_sql(driver, &normalized, &placeholders, kind, &returning_into_names)
        };

        let segments = build_segments(&normalized, &placeholders);

        ParsedSql {
            original_sql: sql.to_string(),
            normalized_sql: normalized,
            segments,
            placeholders,
            kind,
            is_plsql,
            converted_sql,
            rejection_reason: rejection,
            returning_into_names,
        }
    }

    /// Replace bare `RETURN;` with `EXIT rsdb_main;` for PostgreSQL block wrapping.
    pub fn replace_bare_return_for_pg_wrap(sql: &str) -> String {
        let mut out = String::with_capacity(sql.len() + 16);
        let bytes = sql.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if let Some(adv) = try_skip_non_code(sql, i) {
                if adv == 0 {
                    out.push(bytes[i] as char);
                    i += 1;
                } else {
                    out.push_str(&sql[i..i + adv]);
                    i += adv;
                }
                continue;
            }
            if is_word_at(sql, i, "RETURN") {
                let after_kw = i + 6;
                if is_bare_return_statement(sql, after_kw) {
                    out.push_str("EXIT rsdb_main");
                    i = skip_after_bare_return(sql, after_kw);
                    continue;
                }
            }
            out.push(bytes[i] as char);
            i += 1;
        }
        out
    }
}

/// Version tag stored with cached parse results.
pub const PARSER_VERSION: u32 = 1;

fn normalize_sql(sql: &str) -> String {
    let mut s = sql.replace("\r\n", "\n").replace('\r', "\n");
    // Strip SQL*Plus terminator: `/` alone on the last non-empty line.
    let lines: Vec<&str> = s.lines().collect();
    if let Some(last) = lines.iter().rfind(|l| !l.trim().is_empty()) {
        if last.trim() == "/" {
            let mut kept = Vec::new();
            for line in lines {
                if !line.trim().is_empty() || !kept.is_empty() {
                    kept.push(line);
                }
            }
            if kept.last().map(|l| l.trim()) == Some("/") {
                kept.pop();
            }
            s = kept.join("\n");
        }
    }
    s.trim_end().to_string()
}

fn find_placeholders(sql: &str) -> Vec<Placeholder> {
    let mut out = Vec::new();
    let mut i = 0;
    let bytes = sql.as_bytes();
    while i < bytes.len() {
        if let Some(adv) = try_skip_non_code(sql, i) {
            i += adv;
            continue;
        }
        if bytes[i] == b':' {
            if i + 1 < bytes.len() && bytes[i + 1] == b':' {
                i += 2;
                continue;
            }
            if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                i += 2;
                continue;
            }
            let start = i;
            i += 1;
            let name_start = i;
            while i < bytes.len() && is_ident_continue(bytes[i]) {
                i += 1;
            }
            if i > name_start {
                out.push(Placeholder {
                    name: sql[name_start..i].to_string(),
                    start,
                    end: i,
                    pg_index: 0,
                });
            }
            continue;
        }
        if bytes[i] == b'?' {
            i += 1;
            continue;
        }
        i += 1;
    }
    out
}

fn assign_pg_indices(placeholders: &mut [Placeholder]) {
    let mut map: HashMap<String, usize> = HashMap::new();
    let mut next = 1usize;
    for ph in placeholders.iter_mut() {
        let idx = *map.entry(ph.name.clone()).or_insert_with(|| {
            let n = next;
            next += 1;
            n
        });
        ph.pg_index = idx;
    }
}

fn detect_is_plsql(sql: &str, rejection: &mut Option<String>) -> bool {
    let Some(kw) = first_significant_word(sql) else {
        return false;
    };
    match kw.as_str() {
        "DECLARE" => true,
        "BEGIN" => {
            if is_begin_transaction(sql) {
                *rejection = Some(
                    "use RsDataBaseConnect::beginTransaction() instead of BEGIN TRANSACTION"
                        .into(),
                );
                false
            } else if is_begin_semicolon_only(sql) {
                *rejection = Some(
                    "use beginTransaction() instead of standalone BEGIN;".into(),
                );
                false
            } else {
                true
            }
        }
        "COMMIT" | "ROLLBACK" | "SAVEPOINT" => {
            if second_significant_word(sql).is_none() {
                *rejection = Some(format!(
                    "standalone {} is not allowed; use transaction API",
                    kw
                ));
            }
            false
        }
        "START" => {
            if matches!(
                second_significant_word(sql).as_deref(),
                Some("TRANSACTION")
            ) {
                *rejection = Some(
                    "use beginTransaction() instead of START TRANSACTION".into(),
                );
            }
            false
        }
        _ => false,
    }
}

fn classify_kind(
    sql: &str,
    is_plsql: bool,
    placeholders: &[Placeholder],
    rejection: &mut Option<String>,
) -> QueryKind {
    if rejection.is_some() {
        return QueryKind::Rejected;
    }
    if is_plsql {
        return QueryKind::Block;
    }
    let Some(kw) = first_significant_word(sql) else {
        return QueryKind::Rejected;
    };
    match kw.to_ascii_uppercase().as_str() {
        "SELECT" | "WITH" => QueryKind::Select,
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" => {
            if has_returning_into(sql) {
                QueryKind::ReturningInto
            } else {
                QueryKind::Dml
            }
        }
        _ => {
            if !placeholders.is_empty() {
                QueryKind::Dml
            } else {
                QueryKind::Select
            }
        }
    }
}

fn has_returning_into(sql: &str) -> bool {
    let upper = sql.to_ascii_uppercase();
    if !upper.contains("RETURNING") {
        return false;
    }
    find_returning_into_start(sql).is_some()
}

fn extract_returning_into_names(sql: &str) -> Vec<String> {
    let mut names = Vec::new();
    let Some(mut i) = find_returning_into_start(sql) else {
        return names;
    };
    i += 4; // INTO
    loop {
        i = skip_space(sql, i);
        if i >= sql.len() {
            break;
        }
        if sql.as_bytes()[i] == b',' {
            i += 1;
            continue;
        }
        if sql.as_bytes()[i] == b':' {
            i += 1;
            let start = i;
            while i < sql.len() && is_ident_continue(sql.as_bytes()[i]) {
                i += 1;
            }
            if i > start {
                names.push(sql[start..i].to_string());
            }
            continue;
        }
        break;
    }
    names
}

fn find_returning_into_start(sql: &str) -> Option<usize> {
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(adv) = try_skip_non_code(sql, i) {
            i += adv;
            continue;
        }
        if is_word_at(sql, i, "RETURNING") {
            let mut j = i + 9;
            j = skip_space(sql, j);
            // Skip returning column list until INTO at paren depth 0
            let mut depth: i32 = 0;
            while j < bytes.len() {
                if let Some(adv) = try_skip_non_code(sql, j) {
                    j += adv;
                    continue;
                }
                if bytes[j] == b'(' {
                    depth += 1;
                    j += 1;
                    continue;
                }
                if bytes[j] == b')' {
                    depth = depth.saturating_sub(1);
                    j += 1;
                    continue;
                }
                if depth == 0 && is_word_at(sql, j, "INTO") {
                    return Some(j);
                }
                j += 1;
            }
        }
        i += 1;
    }
    None
}

fn find_commit_rollback_in_block(sql: &str) -> Option<String> {
    let mut i = 0;
    let bytes = sql.as_bytes();
    while i < bytes.len() {
        if let Some(adv) = try_skip_non_code(sql, i) {
            i += adv;
            continue;
        }
        if is_word_at(sql, i, "COMMIT") && is_standalone_keyword_stmt(sql, i, 6) {
            return Some("COMMIT inside an anonymous block is not allowed".into());
        }
        if is_word_at(sql, i, "ROLLBACK") && is_standalone_keyword_stmt(sql, i, 8) {
            return Some("ROLLBACK inside an anonymous block is not allowed".into());
        }
        i += 1;
    }
    None
}

fn is_standalone_keyword_stmt(sql: &str, start: usize, len: usize) -> bool {
    let after = skip_space(sql, start + len);
    after >= sql.len() || sql.as_bytes()[after] == b';'
}

fn build_converted_sql(
    driver: DriverKind,
    sql: &str,
    placeholders: &[Placeholder],
    kind: QueryKind,
    returning_into: &[String],
) -> String {
    let mut base = if driver == DriverKind::Pg {
        replace_placeholders_pg(sql, placeholders)
    } else {
        sql.to_string()
    };
    if kind == QueryKind::ReturningInto && driver == DriverKind::Pg {
        base = strip_returning_into_clause(&base);
        let _ = returning_into;
    }
    base
}

fn strip_returning_into_clause(sql: &str) -> String {
    let Some(into_pos) = find_returning_into_start(sql) else {
        return sql.to_string();
    };
    let mut end = into_pos + 4;
    loop {
        end = skip_space(sql, end);
        if end >= sql.len() {
            break;
        }
        let b = sql.as_bytes()[end];
        // Original `:name` or already-converted `$n` after placeholder rewrite.
        if b == b':' {
            end += 1;
            while end < sql.len() && is_ident_continue(sql.as_bytes()[end]) {
                end += 1;
            }
            continue;
        }
        if b == b'$' {
            end += 1;
            while end < sql.len() && sql.as_bytes()[end].is_ascii_digit() {
                end += 1;
            }
            continue;
        }
        if b == b',' {
            end += 1;
            continue;
        }
        break;
    }
    let mut out = String::new();
    out.push_str(&sql[..into_pos]);
    out.push_str(&sql[end..]);
    out
}

fn replace_placeholders_pg(sql: &str, placeholders: &[Placeholder]) -> String {
    if placeholders.is_empty() {
        return sql.to_string();
    }
    let mut out = String::with_capacity(sql.len());
    let mut last = 0;
    for ph in placeholders {
        out.push_str(&sql[last..ph.start]);
        out.push('$');
        out.push_str(&ph.pg_index.to_string());
        last = ph.end;
    }
    out.push_str(&sql[last..]);
    out
}

fn build_segments(sql: &str, placeholders: &[Placeholder]) -> Vec<SqlSegment> {
    let mut segs = Vec::new();
    let mut last = 0;
    for ph in placeholders {
        if ph.start > last {
            segs.push(SqlSegment::Literal {
                start: last,
                end: ph.start,
            });
        }
        segs.push(SqlSegment::Placeholder {
            pg_index: ph.pg_index,
        });
        last = ph.end;
    }
    if last < sql.len() {
        segs.push(SqlSegment::Literal {
            start: last,
            end: sql.len(),
        });
    }
    segs
}

fn first_significant_word(sql: &str) -> Option<String> {
    let mut i = 0;
    while i < sql.len() {
        if let Some(adv) = try_skip_non_code(sql, i) {
            i += adv;
            continue;
        }
        if is_ident_start(sql.as_bytes()[i]) {
            let start = i;
            i += 1;
            while i < sql.len() && is_ident_continue(sql.as_bytes()[i]) {
                i += 1;
            }
            return Some(sql[start..i].to_ascii_uppercase());
        }
        i += 1;
    }
    None
}

fn second_significant_word(sql: &str) -> Option<String> {
    let mut seen = 0;
    let mut i = 0;
    while i < sql.len() {
        if let Some(adv) = try_skip_non_code(sql, i) {
            i += adv;
            continue;
        }
        if is_ident_start(sql.as_bytes()[i]) {
            let start = i;
            i += 1;
            while i < sql.len() && is_ident_continue(sql.as_bytes()[i]) {
                i += 1;
            }
            seen += 1;
            if seen == 2 {
                return Some(sql[start..i].to_ascii_uppercase());
            }
            continue;
        }
        i += 1;
    }
    None
}

fn is_begin_transaction(sql: &str) -> bool {
    let Some(rest) = word_after_begin(sql) else {
        return false;
    };
    rest == "TRANSACTION"
}

fn is_begin_semicolon_only(sql: &str) -> bool {
    let mut i = 0;
    while i < sql.len() {
        if let Some(adv) = try_skip_non_code(sql, i) {
            i += adv;
            continue;
        }
        if is_word_at(sql, i, "BEGIN") {
            let mut j = i + 5;
            j = skip_space(sql, j);
            return j < sql.len() && sql.as_bytes()[j] == b';';
        }
        return false;
    }
    false
}

fn word_after_begin(sql: &str) -> Option<String> {
    let mut i = 0;
    while i < sql.len() {
        if let Some(adv) = try_skip_non_code(sql, i) {
            i += adv;
            continue;
        }
        if is_word_at(sql, i, "BEGIN") {
            let mut j = i + 5;
            j = skip_space(sql, j);
            if j >= sql.len() {
                return None;
            }
            if !is_ident_start(sql.as_bytes()[j]) {
                return None;
            }
            let start = j;
            j += 1;
            while j < sql.len() && is_ident_continue(sql.as_bytes()[j]) {
                j += 1;
            }
            return Some(sql[start..j].to_ascii_uppercase());
        }
        return None;
    }
    None
}

fn is_word_at(sql: &str, i: usize, word: &str) -> bool {
    if !sql[i..].starts_with(word) {
        return false;
    }
    let end = i + word.len();
    if i > 0 && is_ident_continue(sql.as_bytes()[i - 1]) {
        return false;
    }
    if end < sql.len() && is_ident_continue(sql.as_bytes()[end]) {
        return false;
    }
    true
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

fn skip_space(sql: &str, mut i: usize) -> usize {
    while i < sql.len() && sql.as_bytes()[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// Skip comments, strings, identifiers. Returns bytes advanced, or 0 if not at skippable token.
fn try_skip_non_code(sql: &str, i: usize) -> Option<usize> {
    let bytes = sql.as_bytes();
    if i >= bytes.len() {
        return Some(0);
    }
    // Line comment
    if i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'-' {
        let mut j = i + 2;
        while j < bytes.len() && bytes[j] != b'\n' {
            j += 1;
        }
        return Some(j - i);
    }
    // Block comment
    if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
        let mut j = i + 2;
        while j + 1 < bytes.len() {
            if bytes[j] == b'*' && bytes[j + 1] == b'/' {
                return Some(j + 2 - i);
            }
            j += 1;
        }
        return Some(bytes.len() - i);
    }
    // Dollar quoting $tag$...$tag$
    if bytes[i] == b'$' {
        if let Some(tag_end) = sql[i + 1..].find('$') {
            let tag = &sql[i..=i + 1 + tag_end];
            if let Some(rel) = sql[i + tag.len()..].find(tag) {
                return Some(tag.len() + rel + tag.len());
            }
        }
    }
    // Double-quoted identifier
    if bytes[i] == b'"' {
        let mut j = i + 1;
        while j < bytes.len() {
            if bytes[j] == b'"' {
                if j + 1 < bytes.len() && bytes[j + 1] == b'"' {
                    j += 2;
                    continue;
                }
                return Some(j + 1 - i);
            }
            j += 1;
        }
        return Some(bytes.len() - i);
    }
    // Oracle q'[...]' style
    if (bytes[i] == b'q' || bytes[i] == b'Q') && i + 2 < bytes.len() && bytes[i + 1] == b'\'' {
        if let Some(adv) = skip_oracle_q_quote(sql, i) {
            return Some(adv);
        }
    }
    // String literal '
    if bytes[i] == b'\'' {
        return skip_string_literal(sql, i);
    }
    // PostgreSQL E'...'
    if (bytes[i] == b'e' || bytes[i] == b'E') && i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
        return skip_string_literal(sql, i + 1);
    }
    None
}

fn skip_string_literal(sql: &str, i: usize) -> Option<usize> {
    let bytes = sql.as_bytes();
    if bytes[i] != b'\'' {
        return None;
    }
    let mut j = i + 1;
    while j < bytes.len() {
        if bytes[j] == b'\'' {
            if j + 1 < bytes.len() && bytes[j + 1] == b'\'' {
                j += 2;
                continue;
            }
            return Some(j + 1 - i);
        }
        j += 1;
    }
    Some(bytes.len() - i)
}

fn skip_oracle_q_quote(sql: &str, i: usize) -> Option<usize> {
    let bytes = sql.as_bytes();
    if i + 3 >= bytes.len() {
        return None;
    }
    let delim = bytes[i + 2];
    if bytes[i + 1] != b'\'' {
        return None;
    }
    let mut j = i + 3;
    while j < bytes.len() {
        if bytes[j] == delim {
            if j + 1 < bytes.len() && bytes[j + 1] == b'\'' {
                return Some(j + 2 - i);
            }
        }
        j += 1;
    }
    None
}

fn is_bare_return_statement(sql: &str, after_kw: usize) -> bool {
    let j = skip_space(sql, after_kw);
    j < sql.len() && sql.as_bytes()[j] == b';'
}

fn skip_after_bare_return(sql: &str, after_kw: usize) -> usize {
    let mut j = skip_space(sql, after_kw);
    if j < sql.len() && sql.as_bytes()[j] == b';' {
        j += 1;
    }
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_in_string_ignored() {
        let sql = "SELECT ':foo' AS x, :bar FROM t";
        let p = ParsedSql::parse(DriverKind::Pg, sql);
        assert_eq!(p.placeholders.len(), 1);
        assert_eq!(p.placeholders[0].name, "bar");
    }

    #[test]
    fn pg_cast_and_assign_skipped() {
        let sql = "SELECT 1::int, x := :v FROM t WHERE :a = 1";
        let p = ParsedSql::parse(DriverKind::Pg, sql);
        assert_eq!(p.placeholders.len(), 2);
        assert!(p.placeholders.iter().any(|ph| ph.name == "v"));
        assert!(p.placeholders.iter().any(|ph| ph.name == "a"));
    }

    #[test]
    fn json_question_mark_not_placeholder() {
        let sql = "SELECT data ? 'key' FROM t WHERE :id = 1";
        let p = ParsedSql::parse(DriverKind::Pg, sql);
        assert_eq!(p.placeholders.len(), 1);
    }

    #[test]
    fn pg_named_to_dollar() {
        let sql = "SELECT :a, :b, :a";
        let p = ParsedSql::parse(DriverKind::Pg, sql);
        assert_eq!(p.converted_sql, "SELECT $1, $2, $1");
    }

    #[test]
    fn reject_standalone_commit() {
        let p = ParsedSql::parse(DriverKind::Pg, "COMMIT");
        assert_eq!(p.kind, QueryKind::Rejected);
        assert!(p.rejection_reason.is_some());
    }

    #[test]
    fn reject_begin_transaction() {
        let p = ParsedSql::parse(DriverKind::Pg, "BEGIN TRANSACTION");
        assert_eq!(p.kind, QueryKind::Rejected);
    }

    #[test]
    fn block_detect_plsql() {
        let p = ParsedSql::parse(DriverKind::Pg, "BEGIN :x := 1; END;");
        assert_eq!(p.kind, QueryKind::Block);
        assert!(p.is_plsql);
    }

    #[test]
    fn commit_inside_block_rejected() {
        let sql = "BEGIN NULL; COMMIT; END;";
        let p = ParsedSql::parse(DriverKind::Pg, sql);
        assert_eq!(p.kind, QueryKind::Rejected);
    }

    #[test]
    fn returning_into_names() {
        let sql = "INSERT INTO t(a) VALUES(1) RETURNING a INTO :out1, :out2";
        let p = ParsedSql::parse(DriverKind::Oci8, sql);
        assert_eq!(p.kind, QueryKind::ReturningInto);
        assert_eq!(p.returning_into_names, vec!["out1", "out2"]);
    }

    #[test]
    fn returning_into_pg_strips_converted_placeholders() {
        let sql = "INSERT INTO t(name, active) VALUES(:name, :active) RETURNING id INTO :newId";
        let p = ParsedSql::parse(DriverKind::Pg, sql);
        assert_eq!(p.kind, QueryKind::ReturningInto);
        assert_eq!(p.returning_into_names, vec!["newId"]);
        let upper = p.converted_sql.to_ascii_uppercase();
        assert!(
            upper.contains("RETURNING"),
            "RETURNING clause must remain: {}",
            p.converted_sql
        );
        assert!(
            !upper.contains("RETURNING ID INTO") && !upper.contains("RETURNING ID  INTO"),
            "RETURNING ... INTO must be stripped to plain RETURNING: {}",
            p.converted_sql
        );
        assert!(
            !p.converted_sql.trim_end().to_ascii_uppercase().ends_with("INTO"),
            "trailing INTO must be removed: {}",
            p.converted_sql
        );
        assert!(
            !p.converted_sql.contains("$3"),
            "OUT bind must not remain as $n: {}",
            p.converted_sql
        );
        assert!(p.converted_sql.contains("$1") && p.converted_sql.contains("$2"));
    }

    #[test]
    fn normalize_trailing_slash() {
        let sql = "BEGIN NULL; END\n/\n";
        let p = ParsedSql::parse(DriverKind::Oci8, sql);
        assert!(!p.normalized_sql.ends_with('/'));
    }

    #[test]
    fn replace_bare_return() {
        let sql = "BEGIN RETURN; END;";
        let out = ParsedSql::replace_bare_return_for_pg_wrap(sql);
        assert!(out.contains("EXIT rsdb_main"));
        assert!(!out.contains("RETURN;"));
    }

    #[test]
    fn dollar_quoting_skips_colon_inside() {
        let sql = "SELECT $x$ :notbind $x$, :yes";
        let p = ParsedSql::parse(DriverKind::Pg, sql);
        assert_eq!(p.placeholders.len(), 1);
        assert_eq!(p.placeholders[0].name, "yes");
    }
}
