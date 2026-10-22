//! Extension lifecycle hooks and INI registration.
//!
//! | Hook        | Action                                                                      |
//! | ----------- | --------------------------------------------------------------------------- |
//! | `MINIT`     | register INI (`PHP_INI_SYSTEM`), validate, apply `GlobalConfig`, configure parse cache |
//! | `RINIT`     | empty request registry                                                      |
//! | `RSHUTDOWN` | release every lease (registry + orphans), invalidate binds                  |
//! | `MSHUTDOWN` | `pool::shutdown_all()` - close every pooled connection                      |
//!
//! An invalid INI value makes `MINIT` fail, so PHP refuses to load the extension.
//!
//! # Example (php.ini)
//!
//! ```ini
//! extension=rs_database_connect.so
//! rs_database_connect.max_connections_per_process=2
//! rs_database_connect.session_reset=1
//! ```

use crate::config::GlobalConfig;
use crate::php::request;
use crate::pool;
use ext_php_rs::flags::IniEntryPermission;
use ext_php_rs::zend::{ExecutorGlobals, IniEntryDef};
use std::collections::HashMap;
use std::panic::catch_unwind;

/// INI directives: `(name without prefix, default value)`.
pub const INI_DEFAULTS: [(&str, &str); 12] = [
    ("max_connections_per_process", "2"),
    ("pool_wait_timeout_ms", "30000"),
    ("idle_validate_ms", "60000"),
    ("persistent_timeout_s", "0"),
    ("connect_timeout_ms", "10000"),
    ("call_timeout_ms", "0"),
    ("session_reset", "1"),
    ("parse_cache_entries", "512"),
    ("parse_cache_max_bytes", "4194304"),
    ("parse_cache_max_sql_bytes", "65536"),
    ("lob_max_bytes", "16777216"),
    ("stmt_cache_size", "20"),
];

const INI_PREFIX: &str = "rs_database_connect.";

/// Register all INI entries as `PHP_INI_SYSTEM` (not changeable via `ini_set()`).
///
/// # Example
///
/// ```ignore
/// register_ini(module_number);
/// ```
pub fn register_ini(module_number: i32) {
    let entries: Vec<IniEntryDef> = INI_DEFAULTS
        .iter()
        .map(|(name, default)| {
            IniEntryDef::new(
                format!("{}{}", INI_PREFIX, name),
                (*default).to_string(),
                &IniEntryPermission::System,
            )
        })
        .collect();
    IniEntryDef::register(entries, module_number);
}

/// Read the effective INI values (falling back to defaults), validate and apply them.
///
/// # Example
///
/// ```ignore
/// load_config()?; // Err(message) for an invalid value
/// ```
pub fn load_config() -> Result<(), String> {
    let ini = ExecutorGlobals::get().ini_values();
    let mut map: HashMap<String, String> = HashMap::new();
    for (name, default) in INI_DEFAULTS.iter() {
        let full = format!("{}{}", INI_PREFIX, name);
        let value = ini
            .get(&full)
            .and_then(|v| v.clone())
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| (*default).to_string());
        map.insert(full, value);
    }
    GlobalConfig::set_from_ini_map(&map).map_err(|e| e.to_string())
}

/// `MINIT`: INI registration, validation, config + parse-cache configuration.
///
/// Returns `0` on success; non-zero aborts loading of the extension.
///
/// # Example
///
/// ```ignore
/// #[php_module]
/// #[php(startup = "minit")]
/// pub fn get_module(m: ModuleBuilder) -> ModuleBuilder { m }
/// ```
pub fn minit(_ty: i32, module_number: i32) -> i32 {
    let result = catch_unwind(|| {
        crate::php::exception::register_classes()?;
        register_ini(module_number);
        load_config()
    });
    match result {
        Ok(Ok(())) => 0,
        Ok(Err(msg)) => {
            eprintln!("rs_database_connect: {}", msg);
            1
        }
        Err(_) => {
            eprintln!("rs_database_connect: panic during module startup");
            1
        }
    }
}

/// `RINIT`: start with an empty request registry.
pub extern "C" fn rinit(_ty: i32, _module_number: i32) -> i32 {
    let _ = catch_unwind(request::reset);
    0
}

/// `RSHUTDOWN`: release all leases (registry and orphans), invalidate binds, drop object refs.
pub extern "C" fn rshutdown(_ty: i32, _module_number: i32) -> i32 {
    let _ = catch_unwind(request::shutdown);
    0
}

/// `MSHUTDOWN`: close every connection of the process registry.
pub extern "C" fn mshutdown(_ty: i32, _module_number: i32) -> i32 {
    let _ = catch_unwind(pool::shutdown_all);
    0
}

/// `phpinfo()` / `php -i` section.
pub extern "C" fn minfo(_module: *mut ext_php_rs::zend::ModuleEntry) {
    ext_php_rs::info_table_start!();
    ext_php_rs::info_table_row!("rs_database_connect", "enabled");
    ext_php_rs::info_table_row!(
        "Thread safety",
        if cfg!(php_zts) { "ZTS" } else { "NTS" }
    );
    ext_php_rs::info_table_row!(
        "PostgreSQL driver",
        if cfg!(feature = "postgres") { "yes" } else { "no" }
    );
    ext_php_rs::info_table_row!(
        "Oracle driver",
        if cfg!(feature = "oracle") { "yes" } else { "no" }
    );
    ext_php_rs::info_table_end!();
}
