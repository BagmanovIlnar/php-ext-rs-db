//! DSN parsing for Oracle (`oci8`) and PostgreSQL (`pg`).

use crate::error::Error;
use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

/// Database driver kind derived from the DSN scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DriverKind {
    Oci8,
    Pg,
}

impl DriverKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DriverKind::Oci8 => "oci8",
            DriverKind::Pg => "pg",
        }
    }
}

/// TLS protocol for Oracle easy connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OracleProtocol {
    Tcp,
    Tcps,
}

/// PostgreSQL `sslmode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PgSslMode {
    Disable,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

/// Result column name casing policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColumnCase {
    Upper,
    Lower,
    Native,
}

impl Default for ColumnCase {
    fn default() -> Self {
        ColumnCase::Upper
    }
}

/// Oracle connection mode (DRCP vs dedicated).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OracleMode {
    Pooled,
    Dedicated,
}

/// Parsed connection string with normalized query parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dsn {
    pub driver: DriverKind,
    pub user: String,
    pub password: String,
    pub host: String,
    pub port: u16,
    /// Oracle service name or PostgreSQL database name.
    pub service_or_database: String,
    pub oracle_mode: Option<OracleMode>,
    pub connect_timeout_ms: Option<u64>,
    pub call_timeout_ms: Option<u64>,
    pub keepalive_s: Option<u64>,
    pub protocol: Option<OracleProtocol>,
    pub sslmode: Option<PgSslMode>,
    pub sslrootcert: Option<String>,
    pub application_name: Option<String>,
    pub statement_rollback: bool,
    pub column_case: ColumnCase,
}

impl Dsn {
    /// Parse a DSN string. User and password are percent-decoded.
    pub fn parse(input: &str) -> Result<Self, Error> {
        let input = input.trim();
        if input.is_empty() {
            return Err(Error::Dsn("empty DSN".into()));
        }

        let (scheme, rest) = input
            .split_once("://")
            .ok_or_else(|| Error::Dsn("missing ://".into()))?;
        let driver = match scheme.to_ascii_lowercase().as_str() {
            "oci8" => DriverKind::Oci8,
            "pg" => DriverKind::Pg,
            other => return Err(Error::Dsn(format!("unsupported scheme '{}'", other))),
        };

        let (authority_and_path, query) = split_query(rest);
        let query_str = query.unwrap_or("");

        let at = find_userinfo_at(authority_and_path)
            .ok_or_else(|| Error::Dsn("missing @ before host".into()))?;
        let userinfo = &authority_and_path[..at];
        let host_path = &authority_and_path[at + 1..];

        let (user, password) = parse_userinfo(userinfo)?;
        let (host, port, path) = parse_host_port_path(host_path)?;

        let (service_or_database, oracle_mode) = match driver {
            DriverKind::Pg => {
                if path.is_empty() {
                    return Err(Error::Dsn("missing database name".into()));
                }
                if path.contains(':') {
                    return Err(Error::Dsn(
                        "PostgreSQL database name must not contain ':'".into(),
                    ));
                }
                (path.to_string(), None)
            }
            DriverKind::Oci8 => {
                if path.is_empty() {
                    return Err(Error::Dsn("missing service name".into()));
                }
                parse_oracle_service_path(path)?
            }
        };

        let mut params = QueryParams::default();
        if !query_str.is_empty() {
            parse_query_params(query_str, driver, &mut params)?;
        }
        params.apply_defaults(driver);

        Ok(Dsn {
            driver,
            user,
            password,
            host,
            port,
            service_or_database,
            oracle_mode,
            connect_timeout_ms: params.connect_timeout_ms,
            call_timeout_ms: params.call_timeout_ms,
            keepalive_s: params.keepalive_s,
            protocol: params.protocol,
            sslmode: params.sslmode,
            sslrootcert: params.sslrootcert,
            application_name: params.application_name,
            statement_rollback: params.statement_rollback,
            column_case: params.column_case,
        })
    }

    pub fn driver_kind(&self) -> DriverKind {
        self.driver
    }

    /// DSN with password replaced by `***` (for logs and `getAddress()`).
    pub fn masked_address(&self) -> String {
        self.format_dsn(true)
    }

    /// Stable pool key: normalized masked DSN plus password hash (plaintext password never appears).
    pub fn canonical_key(&self) -> String {
        format!(
            "{}#pwd={:016x}",
            self.format_dsn(true),
            hash_password(&self.password)
        )
    }

    fn format_dsn(&self, mask_password: bool) -> String {
        let scheme = self.driver.as_str();
        let pwd = if mask_password {
            "***".to_string()
        } else {
            percent_encode(&self.password)
        };
        let user_enc = percent_encode(&self.user);
        let host = format_host_for_display(&self.host);
        let mut path = String::new();
        match self.driver {
            DriverKind::Pg => {
                path.push('/');
                path.push_str(&self.service_or_database);
            }
            DriverKind::Oci8 => {
                path.push('/');
                path.push_str(&self.service_or_database);
                if let Some(mode) = self.oracle_mode {
                    path.push(':');
                    path.push_str(match mode {
                        OracleMode::Pooled => "pooled",
                        OracleMode::Dedicated => "dedicated",
                    });
                }
            }
        }
        let mut out = format!(
            "{}://{}:{}@{}:{}{}",
            scheme, user_enc, pwd, host, self.port, path
        );
        let qs = self.canonical_query_string();
        if !qs.is_empty() {
            out.push('?');
            out.push_str(&qs);
        }
        out
    }

    fn canonical_query_string(&self) -> String {
        let mut map: BTreeMap<&str, String> = BTreeMap::new();
        if let Some(v) = self.connect_timeout_ms {
            map.insert("connect_timeout_ms", v.to_string());
        }
        if let Some(v) = self.call_timeout_ms {
            map.insert("call_timeout_ms", v.to_string());
        }
        if let Some(v) = self.keepalive_s {
            map.insert("keepalive_s", v.to_string());
        }
        if let Some(p) = self.protocol {
            map.insert(
                "protocol",
                match p {
                    OracleProtocol::Tcp => "tcp".into(),
                    OracleProtocol::Tcps => "tcps".into(),
                },
            );
        }
        if let Some(m) = self.sslmode {
            map.insert(
                "sslmode",
                match m {
                    PgSslMode::Disable => "disable".into(),
                    PgSslMode::Prefer => "prefer".into(),
                    PgSslMode::Require => "require".into(),
                    PgSslMode::VerifyCa => "verify-ca".into(),
                    PgSslMode::VerifyFull => "verify-full".into(),
                },
            );
        }
        if let Some(ref c) = self.sslrootcert {
            map.insert("sslrootcert", c.clone());
        }
        if let Some(ref n) = self.application_name {
            map.insert("application_name", n.clone());
        }
        if self.driver == DriverKind::Pg {
            map.insert(
                "statement_rollback",
                if self.statement_rollback {
                    "on".into()
                } else {
                    "off".into()
                },
            );
        }
        if self.column_case != ColumnCase::default() {
            map.insert(
                "column_case",
                match self.column_case {
                    ColumnCase::Upper => "upper".into(),
                    ColumnCase::Lower => "lower".into(),
                    ColumnCase::Native => "native".into(),
                },
            );
        }
        map.into_iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("&")
    }
}

fn hash_password(password: &str) -> u64 {
    let mut h = DefaultHasher::new();
    password.hash(&mut h);
    h.finish()
}

fn split_query(rest: &str) -> (&str, Option<&str>) {
    if let Some(q) = rest.find('?') {
        (&rest[..q], Some(&rest[q + 1..]))
    } else {
        (rest, None)
    }
}

fn find_userinfo_at(s: &str) -> Option<usize> {
    let mut i = 0;
    let bytes = s.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'@' {
            return Some(i);
        }
        if bytes[i] == b'[' {
            if let Some(end) = s[i..].find(']') {
                i += end + 1;
                continue;
            }
        }
        i += 1;
    }
    None
}

fn parse_userinfo(userinfo: &str) -> Result<(String, String), Error> {
    if userinfo.is_empty() {
        return Err(Error::Dsn("empty user".into()));
    }
    let colon = userinfo.find(':').unwrap_or(userinfo.len());
    let user_raw = &userinfo[..colon];
    let pass_raw = if colon < userinfo.len() {
        &userinfo[colon + 1..]
    } else {
        ""
    };
    Ok((percent_decode(user_raw)?, percent_decode(pass_raw)?))
}

fn parse_host_port_path(host_path: &str) -> Result<(String, u16, &str), Error> {
    let (host_port, path) = host_path
        .split_once('/')
        .map(|(hp, p)| (hp, p))
        .unwrap_or((host_path, ""));

    let (host, port_str) = if host_port.starts_with('[') {
        let end = host_port
            .find(']')
            .ok_or_else(|| Error::Dsn("unclosed IPv6 host".into()))?;
        let host = host_port[1..end].to_string();
        let rest = &host_port[end + 1..];
        if !rest.starts_with(':') {
            return Err(Error::Dsn("missing port after IPv6 host".into()));
        }
        (host, &rest[1..])
    } else {
        let colon = host_port
            .rfind(':')
            .ok_or_else(|| Error::Dsn("missing port".into()))?;
        (host_port[..colon].to_string(), &host_port[colon + 1..])
    };

    if host.is_empty() {
        return Err(Error::Dsn("empty host".into()));
    }
    let port: u16 = port_str
        .parse()
        .map_err(|_| Error::Dsn("invalid port".into()))?;
    Ok((host, port, path))
}

fn parse_oracle_service_path(path: &str) -> Result<(String, Option<OracleMode>), Error> {
    if let Some(idx) = path.rfind(':') {
        let service = &path[..idx];
        let mode_str = &path[idx + 1..];
        if service.is_empty() {
            return Err(Error::Dsn("empty service name".into()));
        }
        let mode = parse_oracle_mode(mode_str)?;
        return Ok((service.to_string(), Some(mode)));
    }
    Ok((path.to_string(), None))
}

fn parse_oracle_mode(s: &str) -> Result<OracleMode, Error> {
    match s.to_ascii_lowercase().as_str() {
        "pooled" => Ok(OracleMode::Pooled),
        "dedicated" => Ok(OracleMode::Dedicated),
        other => Err(Error::Dsn(format!(
            "unknown Oracle mode '{}', expected pooled or dedicated",
            other
        ))),
    }
}

#[derive(Default)]
struct QueryParams {
    connect_timeout_ms: Option<u64>,
    call_timeout_ms: Option<u64>,
    keepalive_s: Option<u64>,
    protocol: Option<OracleProtocol>,
    sslmode: Option<PgSslMode>,
    sslrootcert: Option<String>,
    application_name: Option<String>,
    statement_rollback: bool,
    column_case: ColumnCase,
    statement_rollback_set: bool,
    column_case_set: bool,
}

impl QueryParams {
    fn apply_defaults(&mut self, driver: DriverKind) {
        if driver == DriverKind::Pg && !self.statement_rollback_set {
            self.statement_rollback = true;
        }
        if !self.column_case_set {
            self.column_case = ColumnCase::Upper;
        }
    }
}

fn parse_query_params(
    query: &str,
    driver: DriverKind,
    params: &mut QueryParams,
) -> Result<(), Error> {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| Error::Dsn(format!("invalid query parameter '{}'", pair)))?;
        if key.is_empty() {
            return Err(Error::Dsn("empty query parameter name".into()));
        }
        let key_lc = key.to_ascii_lowercase();
        match key_lc.as_str() {
            "connect_timeout_ms" => {
                params.connect_timeout_ms = Some(parse_u64_param(value, key)?);
            }
            "call_timeout_ms" => {
                params.call_timeout_ms = Some(parse_u64_param(value, key)?);
            }
            "keepalive_s" => {
                params.keepalive_s = Some(parse_u64_param(value, key)?);
            }
            "protocol" => {
                if driver != DriverKind::Oci8 {
                    return Err(Error::Dsn(format!(
                        "parameter '{}' is only valid for oci8",
                        key
                    )));
                }
                params.protocol = Some(match value.to_ascii_lowercase().as_str() {
                    "tcp" => OracleProtocol::Tcp,
                    "tcps" => OracleProtocol::Tcps,
                    other => {
                        return Err(Error::Dsn(format!(
                            "invalid protocol '{}', expected tcp or tcps",
                            other
                        )))
                    }
                });
            }
            "sslmode" => {
                if driver != DriverKind::Pg {
                    return Err(Error::Dsn(format!(
                        "parameter '{}' is only valid for pg",
                        key
                    )));
                }
                params.sslmode = Some(match value.to_ascii_lowercase().as_str() {
                    "disable" => PgSslMode::Disable,
                    "prefer" => PgSslMode::Prefer,
                    "require" => PgSslMode::Require,
                    "verify-ca" => PgSslMode::VerifyCa,
                    "verify-full" => PgSslMode::VerifyFull,
                    other => {
                        return Err(Error::Dsn(format!("invalid sslmode '{}'", other)))
                    }
                });
            }
            "sslrootcert" => {
                if driver != DriverKind::Pg {
                    return Err(Error::Dsn(format!(
                        "parameter '{}' is only valid for pg",
                        key
                    )));
                }
                params.sslrootcert = Some(value.to_string());
            }
            "application_name" => {
                if driver != DriverKind::Pg {
                    return Err(Error::Dsn(format!(
                        "parameter '{}' is only valid for pg",
                        key
                    )));
                }
                params.application_name = Some(value.to_string());
            }
            "statement_rollback" => {
                if driver != DriverKind::Pg {
                    return Err(Error::Dsn(format!(
                        "parameter '{}' is only valid for pg",
                        key
                    )));
                }
                params.statement_rollback_set = true;
                params.statement_rollback = match value.to_ascii_lowercase().as_str() {
                    "on" => true,
                    "off" => false,
                    other => {
                        return Err(Error::Dsn(format!(
                            "invalid statement_rollback '{}', expected on or off",
                            other
                        )))
                    }
                };
            }
            "column_case" => {
                params.column_case_set = true;
                params.column_case = match value.to_ascii_lowercase().as_str() {
                    "upper" => ColumnCase::Upper,
                    "lower" => ColumnCase::Lower,
                    "native" => ColumnCase::Native,
                    other => {
                        return Err(Error::Dsn(format!(
                            "invalid column_case '{}'",
                            other
                        )))
                    }
                };
            }
            other => return Err(Error::Dsn(format!("unknown query parameter '{}'", other))),
        }
    }
    Ok(())
}

fn parse_u64_param(value: &str, name: &str) -> Result<u64, Error> {
    value
        .parse::<u64>()
        .map_err(|_| Error::Dsn(format!("invalid numeric value for '{}'", name)))
}

fn format_host_for_display(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{}]", host)
    } else {
        host.to_string()
    }
}

fn percent_decode(input: &str) -> Result<String, Error> {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(Error::Dsn("invalid percent-encoding".into()));
            }
            let hi = hex_val(bytes[i + 1])?;
            let lo = hex_val(bytes[i + 2])?;
            out.push(char::from((hi << 4) | lo));
            i += 3;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    Ok(out)
}

fn hex_val(b: u8) -> Result<u8, Error> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(Error::Dsn("invalid percent-encoding".into())),
    }
}

fn percent_encode(input: &str) -> String {
    let mut out = String::new();
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pg_basic() {
        let d = Dsn::parse("pg://u:p@localhost:5432/mydb").unwrap();
        assert_eq!(d.driver, DriverKind::Pg);
        assert_eq!(d.user, "u");
        assert_eq!(d.password, "p");
        assert_eq!(d.host, "localhost");
        assert_eq!(d.port, 5432);
        assert_eq!(d.service_or_database, "mydb");
        assert!(d.statement_rollback);
        assert_eq!(d.column_case, ColumnCase::Upper);
    }

    #[test]
    fn parse_pg_percent_decode() {
        let d = Dsn::parse("pg://user%40x:pa%3Ass@host:5432/db").unwrap();
        assert_eq!(d.user, "user@x");
        assert_eq!(d.password, "pa:ss");
    }

    #[test]
    fn parse_pg_ipv6() {
        let d = Dsn::parse("pg://u:p@[::1]:5432/db").unwrap();
        assert_eq!(d.host, "::1");
        assert_eq!(d.port, 5432);
    }

    #[test]
    fn parse_oci8_modes_case_insensitive() {
        let d = Dsn::parse("oci8://u:p@h:1521/svc:POOLED").unwrap();
        assert_eq!(d.oracle_mode, Some(OracleMode::Pooled));
        let d2 = Dsn::parse("oci8://u:p@h:1521/svc:Dedicated").unwrap();
        assert_eq!(d2.oracle_mode, Some(OracleMode::Dedicated));
    }

    #[test]
    fn masked_hides_password() {
        let d = Dsn::parse("pg://u:secret@h:5432/db").unwrap();
        let m = d.masked_address();
        assert!(!m.contains("secret"));
        assert!(m.contains("***"));
        assert!(m.contains("u:"));
    }

    #[test]
    fn canonical_key_differs_by_password() {
        let a = Dsn::parse("pg://u:one@h:5432/db").unwrap();
        let b = Dsn::parse("pg://u:two@h:5432/db").unwrap();
        assert_ne!(a.canonical_key(), b.canonical_key());
        assert!(!a.canonical_key().contains("one"));
        assert!(!a.canonical_key().contains("two"));
    }

    #[test]
    fn query_params_and_unknown() {
        let d = Dsn::parse(
            "pg://u:p@h:5432/db?sslmode=require&statement_rollback=off&column_case=lower",
        )
        .unwrap();
        assert_eq!(d.sslmode, Some(PgSslMode::Require));
        assert!(!d.statement_rollback);
        assert_eq!(d.column_case, ColumnCase::Lower);
        let err = Dsn::parse("pg://u:p@h:5432/db?foo=bar").unwrap_err();
        assert!(matches!(err, Error::Dsn(_)));
    }

    #[test]
    fn oci8_protocol() {
        let d = Dsn::parse("oci8://u:p@h:1521/svc:pooled?protocol=tcps").unwrap();
        assert_eq!(d.protocol, Some(OracleProtocol::Tcps));
    }

    #[test]
    fn equivalent_canonical_except_password_hash() {
        let d1 = Dsn::parse("pg://u:p@h:5432/db?statement_rollback=on").unwrap();
        let d2 = Dsn::parse("pg://u:p@h:5432/db").unwrap();
        let k1 = d1.canonical_key();
        let k2 = d2.canonical_key();
        assert_eq!(k1, k2);
    }
}
