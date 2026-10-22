# Docker helpers for `rs_database_connect`

## Install into a host PHP

```bash
export PHP_CONFIG=/usr/bin/php-config
./docker/php/install-rs-database-connect.sh
```

Optional: `RSDB_FEATURES=postgres,oracle` `RSDB_PROFILE=release`.

## Image build

```bash
docker build -f docker/php/Dockerfile --build-arg PHP_VERSION=8.3 -t rsdb-php:8.3 .
docker run --rm rsdb-php:8.3 php -m | grep rs_database
```

Runtime image contains only the `.so` and OpenSSL — not the Rust toolchain.

## Connection load on the database

`max_connections_per_process` is **per PHP worker**. Approximate ceiling:

```text
pm.max_children × max_connections_per_process × distinct DSN count
```

Use Oracle DRCP (`:pooled`) or PgBouncer for a server-wide pool. For PgBouncer transaction mode set `rs_database_connect.session_reset=0`.
