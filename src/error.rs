//! Error types for `rs_database_connect`. Display output must never include passwords.

use std::fmt;

/// Top-level error enum for the extension core (Rust side).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Dsn(String),
    Connection(String),
    PoolExhausted,
    PoolTimeout,
    Sql(String),
    Bind(String),
    Driver(String),
    Internal(String),
}

impl Error {
    /// Sanitize a message that might accidentally contain a password substring.
    pub fn sanitize_message(msg: &str, password: Option<&str>) -> String {
        let mut out = msg.to_string();
        if let Some(pwd) = password {
            if !pwd.is_empty() {
                out = out.replace(pwd, "***");
            }
        }
        out
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Dsn(msg) => write!(f, "Invalid DSN: {}", msg),
            Error::Connection(msg) => write!(f, "Connection error: {}", msg),
            Error::PoolExhausted => write!(
                f,
                "Connection pool exhausted for this DSN in this process"
            ),
            Error::PoolTimeout => write!(f, "Timed out waiting for a free connection slot"),
            Error::Sql(msg) => write!(f, "SQL error: {}", msg),
            Error::Bind(msg) => write!(f, "Bind error: {}", msg),
            Error::Driver(msg) => write!(f, "Driver error: {}", msg),
            Error::Internal(msg) => write!(f, "Internal error: {}", msg),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_variants_no_secrets() {
        let e = Error::Dsn("bad scheme".into());
        assert_eq!(format!("{}", e), "Invalid DSN: bad scheme");
        assert_eq!(
            format!("{}", Error::PoolExhausted),
            "Connection pool exhausted for this DSN in this process"
        );
    }

    #[test]
    fn sanitize_strips_password() {
        let msg = "failed for user secret123@host";
        assert_eq!(
            Error::sanitize_message(msg, Some("secret123")),
            "failed for user ***@host"
        );
    }
}
