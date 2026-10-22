# rs_database_connect

Synchronous PHP 8.2–8.5 extension (Rust / `ext-php-rs`) for **Oracle** and **PostgreSQL**:

| Class | Role |
| --- | --- |
| `RsDataBaseConnect` | Lease of a physical connection, process-local persistent pool, transactions |
| `RsDataBasePrepare` | SQL, binds, execute, fetch |
| `RsDataBaseBind` | One bind parameter (optional IN/OUT by reference) |
| `RsDataBaseCursor` | OUT REF CURSOR / refcursor result |

API mirrors capabilities of a Kernel `DataBase/Query` layer without depending on Kernel classes.

Design plan: [`RS_DATABASE_CONNECT_PLAN_RUST_HEARTBEAT.md`](RS_DATABASE_CONNECT_PLAN_RUST_HEARTBEAT.md).

## Features

- Named binds (`:id` / `id`), SQL lexer (not regex) with parse cache
- Persistent pool **per PHP process** (same model as `oci_pconnect` / `pg_pconnect`)
- Nested transactions via savepoints; PostgreSQL statement savepoints for Oracle-like error isolation
- PostgreSQL anonymous blocks with IN/OUT via transactional `set_config` + generated `DO`
- Password never appears in diagnostics (`***` in addresses / errors)
- No background heartbeat threads by default

## Requirements

| Component | Notes |
| --- | --- |
| PHP | 8.2–8.5 NTS (ZTS = separate build) |
| Rust | 1.75+ (CI uses stable) |
| OpenSSL | For PostgreSQL TLS (`postgres-openssl`) |
| Oracle Instant Client | Only when building with `--features oracle` |

## Prebuilt releases

GitHub Releases (`v*`) attach archives:

- Linux/macOS: `rs_database_connect-<ver>-<os>-<arch>-php<x.y>-nts-postgres.tar.gz`
- Windows: `rs_database_connect-<ver>-windows-x86_64-php<x.y>-nts-postgres.zip`

Inside: `rs_database_connect.so` / `.dylib` / `.dll`, ini snippet, stub, `INSTALL.txt`.

## Build

```bash
export PHP_CONFIG="$(which php-config)"   # must match target PHP
cargo build --release --features postgres
# PostgreSQL + Oracle (ODPI-C linked via `oracle` crate; Instant Client needed at runtime):
cargo build --release --features postgres,oracle
```

Artifact (macOS): `target/release/librs_database_connect.dylib`  
Artifact (Linux): `target/release/librs_database_connect.so`

Install:

```bash
# copy into extension_dir, then enable
extension=rs_database_connect
```

Recommended production INI (see plan §4):

```ini
extension=rs_database_connect
zend.exception_ignore_args=On
rs_database_connect.max_connections_per_process=2
rs_database_connect.idle_validate_ms=60000
rs_database_connect.session_reset=1
rs_database_connect.lob_max_bytes=16777216
rs_database_connect.stmt_cache_size=20
```

**SAPI:** first release supports **NTS only** (typical PHP-FPM). ZTS builds refuse to load.

**Runtime:** OpenSSL for PostgreSQL TLS; Oracle Instant Client when built with `--features oracle`.

## DSN

```text
oci8://user:password@host:port/service:pooled
oci8://user:password@host:port/service:dedicated
pg://user:password@host:port/database
```

Query parameters (unknown keys are errors): `connect_timeout_ms`, `call_timeout_ms`, `keepalive_s`, `protocol`, `sslmode`, `sslrootcert`, `application_name`, `statement_rollback`, `column_case`.

## Quick example

```php
$db = RsDataBaseConnect::getInstance('pg://DEV:secret@127.0.0.1:5432/DEV', true);

$q = $db->prepare('SELECT id, name FROM organisation WHERE active = :active');
$q->bindValue('active', true, -1, RsDataBasePrepare::BOOLEAN);

if (!$q->execute()) {
    throw new RsDataBaseException($q->getErrorMessage());
}

$rows = $q->fetchAllRows();
$db->close();
```

IDE stubs: [`stubs/rs_database_connect.stub.php`](stubs/rs_database_connect.stub.php).

## Tests

```bash
cargo test --lib --features postgres
# PHPT (requires built extension + DSN env):
# RSDB_DSN='pg://…' RSDB_EXT=/path/to/librs_database_connect.so php run-tests.php tests/phpt
```

## Docker / CI

- `docker/php/` — multi-stage build helpers
- `.github/workflows/ci.yml` — Rust unit tests + PHP matrix build smoke

## License

MIT — see [LICENSE](LICENSE).
