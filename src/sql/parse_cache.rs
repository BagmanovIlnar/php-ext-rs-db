//! Process-global LRU cache for parsed SQL.

use crate::dsn::DriverKind;
use crate::sql::tokenizer::{ParsedSql, PARSER_VERSION};
use std::collections::{HashMap, VecDeque};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

/// Cache statistics exposed to PHP diagnostics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ParseCacheStats {
    pub entries: usize,
    pub bytes: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
}

struct CacheEntry {
    driver: DriverKind,
    sql: String,
    parsed: ParsedSql,
    parser_version: u32,
    byte_size: usize,
}

struct ParseCacheInner {
    map: HashMap<u128, CacheEntry>,
    order: VecDeque<u128>,
    max_entries: usize,
    max_bytes: usize,
    max_sql_bytes: usize,
    total_bytes: usize,
    hits: u64,
    misses: u64,
    evictions: u64,
}

impl ParseCacheInner {
    fn new(max_entries: usize, max_bytes: usize, max_sql_bytes: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            max_entries,
            max_bytes,
            max_sql_bytes,
            total_bytes: 0,
            hits: 0,
            misses: 0,
            evictions: 0,
        }
    }

    fn entry_bytes(sql: &str, parsed: &ParsedSql) -> usize {
        sql.len() + parsed.normalized_sql.len() + parsed.converted_sql.len() + 64
    }

    fn get_or_parse(&mut self, driver: DriverKind, sql: &str) -> ParsedSql {
        if self.max_entries == 0 {
            self.misses += 1;
            return ParsedSql::parse(driver, sql);
        }

        let key = cache_key(driver, sql);
        if let Some(entry) = self.map.get(&key) {
            if entry.parser_version == PARSER_VERSION
                && entry.driver == driver
                && entry.sql == sql
            {
                self.hits += 1;
                let parsed = entry.parsed.clone();
                self.touch(key);
                return parsed;
            }
        }

        self.misses += 1;
        let parsed = ParsedSql::parse(driver, sql);

        if sql.len() > self.max_sql_bytes {
            return parsed;
        }

        let byte_size = Self::entry_bytes(sql, &parsed);
        if byte_size > self.max_bytes && self.max_entries > 0 {
            return parsed;
        }

        if self.map.contains_key(&key) {
            if let Some(old) = self.map.remove(&key) {
                self.total_bytes = self.total_bytes.saturating_sub(old.byte_size);
                self.order.retain(|k| k != &key);
            }
        }

        self.evict_for(byte_size);

        let entry = CacheEntry {
            driver,
            sql: sql.to_string(),
            parsed: parsed.clone(),
            parser_version: PARSER_VERSION,
            byte_size,
        };
        self.total_bytes += byte_size;
        self.map.insert(key, entry);
        self.order.push_back(key);
        parsed
    }

    fn touch(&mut self, key: u128) {
        if let Some(pos) = self.order.iter().position(|k| *k == key) {
            self.order.remove(pos);
            self.order.push_back(key);
        }
    }

    fn evict_for(&mut self, incoming: usize) {
        while (self.map.len() >= self.max_entries && self.max_entries > 0)
            || (self.total_bytes + incoming > self.max_bytes && self.max_bytes > 0)
        {
            if let Some(old_key) = self.order.pop_front() {
                if let Some(old) = self.map.remove(&old_key) {
                    self.total_bytes = self.total_bytes.saturating_sub(old.byte_size);
                    self.evictions += 1;
                }
            } else {
                break;
            }
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.total_bytes = 0;
    }

    fn stats(&self) -> ParseCacheStats {
        ParseCacheStats {
            entries: self.map.len(),
            bytes: self.total_bytes,
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
        }
    }
}

fn cache_key(driver: DriverKind, sql: &str) -> u128 {
    let mut h1 = DefaultHasher::new();
    driver.hash(&mut h1);
    sql.hash(&mut h1);
    let a = h1.finish();

    let mut h2 = DefaultHasher::new();
    PARSER_VERSION.hash(&mut h2);
    sql.hash(&mut h2);
    driver.hash(&mut h2);
    let b = h2.finish();

    ((a as u128) << 64) | (b as u128)
}

static PARSE_CACHE: Mutex<Option<ParseCacheInner>> = Mutex::new(None);

fn with_cache<F, R>(f: F) -> R
where
    F: FnOnce(&mut ParseCacheInner) -> R,
{
    let mut guard = PARSE_CACHE.lock().expect("parse cache mutex poisoned");
    if guard.is_none() {
        *guard = Some(ParseCacheInner::new(512, 4 * 1024 * 1024, 65536));
    }
    f(guard.as_mut().unwrap())
}

/// Configure cache limits (typically from INI at MINIT).
pub fn configure(max_entries: usize, max_bytes: usize, max_sql_bytes: usize) {
    let mut guard = PARSE_CACHE.lock().expect("parse cache mutex poisoned");
    *guard = Some(ParseCacheInner::new(max_entries, max_bytes, max_sql_bytes));
}

/// Parse SQL using the process-global cache.
pub fn parse_cached(driver: DriverKind, sql: &str) -> ParsedSql {
    with_cache(|c| c.get_or_parse(driver, sql))
}

/// Clear all cached entries for this process.
pub fn clear() {
    with_cache(|c| c.clear());
}

/// Return current cache statistics.
pub fn stats() -> ParseCacheStats {
    with_cache(|c| c.stats())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::tokenizer::QueryKind;
    use std::sync::Mutex;

    /// Global parse cache is process-static; tests must not run concurrently.
    static TEST_SERIAL: Mutex<()> = Mutex::new(());

    fn reset_cache(entries: usize, max_bytes: usize, max_sql: usize) {
        configure(entries, max_bytes, max_sql);
        clear();
    }

    #[test]
    fn hit_and_miss() {
        let _guard = TEST_SERIAL.lock().unwrap();
        reset_cache(16, 1024 * 1024, 65536);
        let sql = "SELECT :a";
        let p1 = parse_cached(DriverKind::Pg, sql);
        let s1 = stats();
        assert_eq!(s1.misses, 1);
        assert_eq!(s1.hits, 0);
        let p2 = parse_cached(DriverKind::Pg, sql);
        let s2 = stats();
        assert_eq!(s2.hits, 1);
        assert_eq!(p1.converted_sql, p2.converted_sql);
    }

    #[test]
    fn collision_safe() {
        let _guard = TEST_SERIAL.lock().unwrap();
        reset_cache(16, 1024 * 1024, 65536);
        // Force different SQL even if hash collided — store compares full SQL.
        let a = parse_cached(DriverKind::Pg, "SELECT 1");
        let b = parse_cached(DriverKind::Pg, "SELECT 2");
        assert_ne!(a.converted_sql, b.converted_sql);
    }

    #[test]
    fn lru_eviction_by_entries() {
        let _guard = TEST_SERIAL.lock().unwrap();
        reset_cache(2, 1024 * 1024, 65536);
        parse_cached(DriverKind::Pg, "SELECT 1");
        parse_cached(DriverKind::Pg, "SELECT 2");
        parse_cached(DriverKind::Pg, "SELECT 3");
        let s = stats();
        assert!(s.evictions >= 1);
        assert!(s.entries <= 2);
    }

    #[test]
    fn skip_oversized_sql() {
        let _guard = TEST_SERIAL.lock().unwrap();
        reset_cache(8, 1024 * 1024, 10);
        let big = "SELECT ".to_string() + &"x".repeat(20);
        parse_cached(DriverKind::Pg, &big);
        assert_eq!(stats().entries, 0);
    }

    #[test]
    fn cache_disabled_with_zero_entries() {
        let _guard = TEST_SERIAL.lock().unwrap();
        reset_cache(0, 1024, 65536);
        parse_cached(DriverKind::Pg, "SELECT 1");
        assert_eq!(stats().entries, 0);
        assert_eq!(stats().misses, 1);
    }

    #[test]
    fn caches_rejected_sql() {
        let _guard = TEST_SERIAL.lock().unwrap();
        reset_cache(8, 1024 * 1024, 65536);
        let sql = "COMMIT";
        let p1 = parse_cached(DriverKind::Pg, sql);
        assert_eq!(p1.kind, QueryKind::Rejected);
        let before = stats().hits;
        let p2 = parse_cached(DriverKind::Pg, sql);
        assert_eq!(p2.kind, QueryKind::Rejected);
        assert_eq!(stats().hits, before + 1);
    }

    #[test]
    fn parser_version_constant() {
        let _guard = TEST_SERIAL.lock().unwrap();
        assert_eq!(PARSER_VERSION, 1);
    }
}
