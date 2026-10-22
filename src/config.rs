//! Process-wide INI settings (thread-safe global).

use crate::error::Error;
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use std::collections::HashMap;

/// Session reset mode when returning a persistent slot to the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionResetMode {
    Off = 0,
    Full = 1,
    Soft = 2,
}

impl SessionResetMode {
    pub fn from_u8(v: u8) -> Result<Self, Error> {
        match v {
            0 => Ok(SessionResetMode::Off),
            1 => Ok(SessionResetMode::Full),
            2 => Ok(SessionResetMode::Soft),
            other => Err(Error::Internal(format!(
                "invalid session_reset value {}",
                other
            ))),
        }
    }
}

/// Extension INI defaults and runtime values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub max_connections_per_process: usize,
    pub pool_wait_timeout_ms: u64,
    pub idle_validate_ms: u64,
    pub persistent_timeout_s: u64,
    pub connect_timeout_ms: u64,
    pub call_timeout_ms: u64,
    pub session_reset: SessionResetMode,
    pub parse_cache_entries: usize,
    pub parse_cache_max_bytes: usize,
    pub parse_cache_max_sql_bytes: usize,
    /// Max bytes when reading/writing CLOB/BLOB as a whole (plan open question 3).
    pub lob_max_bytes: usize,
    /// Max server-side / ODPI statement cache entries per connection.
    pub stmt_cache_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_connections_per_process: 2,
            pool_wait_timeout_ms: 30_000,
            idle_validate_ms: 60_000,
            persistent_timeout_s: 0,
            connect_timeout_ms: 10_000,
            call_timeout_ms: 0,
            session_reset: SessionResetMode::Full,
            parse_cache_entries: 512,
            parse_cache_max_bytes: 4_194_304,
            parse_cache_max_sql_bytes: 65_536,
            lob_max_bytes: 16_777_216,
            stmt_cache_size: 20,
        }
    }
}

impl Config {
    /// Validate fields; returns `Error::Internal` on invalid combination.
    pub fn validate(&self) -> Result<(), Error> {
        if self.max_connections_per_process == 0 {
            return Err(Error::Internal(
                "max_connections_per_process must be >= 1".into(),
            ));
        }
        Ok(())
    }

    fn apply_ini_key(&mut self, key: &str, value: &str) -> Result<(), Error> {
        let key = strip_ini_prefix(key);
        match key {
            "max_connections_per_process" => {
                self.max_connections_per_process = parse_usize(value, key)?;
            }
            "pool_wait_timeout_ms" => {
                self.pool_wait_timeout_ms = parse_u64(value, key)?;
            }
            "idle_validate_ms" => {
                self.idle_validate_ms = parse_u64(value, key)?;
            }
            "persistent_timeout_s" => {
                self.persistent_timeout_s = parse_u64(value, key)?;
            }
            "connect_timeout_ms" => {
                self.connect_timeout_ms = parse_u64(value, key)?;
            }
            "call_timeout_ms" => {
                self.call_timeout_ms = parse_u64(value, key)?;
            }
            "session_reset" => {
                let v: u8 = parse_u64(value, key)?
                    .try_into()
                    .map_err(|_| Error::Internal(format!("invalid {}", key)))?;
                self.session_reset = SessionResetMode::from_u8(v)?;
            }
            "parse_cache_entries" => {
                self.parse_cache_entries = parse_usize(value, key)?;
            }
            "parse_cache_max_bytes" => {
                self.parse_cache_max_bytes = parse_usize(value, key)?;
            }
            "parse_cache_max_sql_bytes" => {
                self.parse_cache_max_sql_bytes = parse_usize(value, key)?;
            }
            "lob_max_bytes" => {
                self.lob_max_bytes = parse_usize(value, key)?;
            }
            "stmt_cache_size" => {
                self.stmt_cache_size = parse_usize(value, key)?;
            }
            unknown => {
                return Err(Error::Internal(format!("unknown INI key '{}'", unknown)));
            }
        }
        Ok(())
    }
}

fn strip_ini_prefix(key: &str) -> &str {
    const PREFIX: &str = "rs_database_connect.";
    if let Some(rest) = key.strip_prefix(PREFIX) {
        rest
    } else {
        key
    }
}

fn parse_u64(value: &str, name: &str) -> Result<u64, Error> {
    value
        .trim()
        .parse::<u64>()
        .map_err(|_| Error::Internal(format!("invalid numeric value for '{}'", name)))
}

fn parse_usize(value: &str, name: &str) -> Result<usize, Error> {
    let v = parse_u64(value, name)?;
    usize::try_from(v).map_err(|_| Error::Internal(format!("value too large for '{}'", name)))
}

static GLOBAL_CONFIG: Lazy<RwLock<Config>> = Lazy::new(|| RwLock::new(Config::default()));

/// Global configuration access.
pub struct GlobalConfig;

impl GlobalConfig {
    /// Current snapshot of process settings.
    pub fn get() -> Config {
        GLOBAL_CONFIG.read().clone()
    }

    /// Replace settings from an INI map (`key` → `value`). Validates before commit.
    pub fn set_from_ini_map(map: &HashMap<String, String>) -> Result<(), Error> {
        let mut next = Config::default();
        for (k, v) in map {
            next.apply_ini_key(k, v)?;
        }
        next.validate()?;
        apply_parse_cache(&next);
        *GLOBAL_CONFIG.write() = next;
        Ok(())
    }

    /// Validate and store an explicit configuration (tests / programmatic setup).
    pub fn set(config: Config) -> Result<(), Error> {
        config.validate()?;
        apply_parse_cache(&config);
        *GLOBAL_CONFIG.write() = config;
        Ok(())
    }
}

fn apply_parse_cache(cfg: &Config) {
    crate::sql::parse_cache::configure(
        cfg.parse_cache_entries,
        cfg.parse_cache_max_bytes,
        cfg.parse_cache_max_sql_bytes,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let c = Config::default();
        assert_eq!(c.max_connections_per_process, 2);
        assert_eq!(c.session_reset, SessionResetMode::Full);
    }

    #[test]
    fn ini_map_with_prefix() {
        let mut m = HashMap::new();
        m.insert(
            "rs_database_connect.max_connections_per_process".into(),
            "5".into(),
        );
        GlobalConfig::set_from_ini_map(&m).unwrap();
        assert_eq!(GlobalConfig::get().max_connections_per_process, 5);
        GlobalConfig::set(Config::default()).unwrap();
    }

    #[test]
    fn rejects_zero_max_connections() {
        let mut c = Config::default();
        c.max_connections_per_process = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn session_reset_bounds() {
        assert!(SessionResetMode::from_u8(3).is_err());
    }
}
