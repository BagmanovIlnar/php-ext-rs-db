#!/usr/bin/env bash
# Build and install rs_database_connect into the current PHP's extension_dir.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PHP_CONFIG="${PHP_CONFIG:-php-config}"
FEATURES="${RSDB_FEATURES:-postgres}"
PROFILE="${RSDB_PROFILE:-release}"

if ! command -v "$PHP_CONFIG" >/dev/null 2>&1; then
  echo "php-config not found; set PHP_CONFIG" >&2
  exit 1
fi

EXT_DIR="$("$PHP_CONFIG" --extension-dir)"
PHP_VER="$("$PHP_CONFIG" --version)"
echo "Building rs_database_connect for PHP ${PHP_VER} → ${EXT_DIR}"

cd "$ROOT"
cargo build --"${PROFILE}" --features "${FEATURES}"

case "$(uname -s)" in
  Darwin) ARTIFACT="target/${PROFILE}/librs_database_connect.dylib" ;;
  *)      ARTIFACT="target/${PROFILE}/librs_database_connect.so" ;;
esac

if [[ ! -f "$ARTIFACT" ]]; then
  echo "missing artifact: $ARTIFACT" >&2
  exit 1
fi

install -m 0755 "$ARTIFACT" "${EXT_DIR}/rs_database_connect.so"

INI_DIR="$("$PHP_CONFIG" --ini-dir 2>/dev/null || true)"
if [[ -n "${INI_DIR}" && -d "${INI_DIR}" ]]; then
  cat > "${INI_DIR}/rs_database_connect.ini" <<'EOF'
extension=rs_database_connect
zend.exception_ignore_args=On
rs_database_connect.max_connections_per_process=2
rs_database_connect.pool_wait_timeout_ms=30000
rs_database_connect.idle_validate_ms=60000
rs_database_connect.persistent_timeout_s=0
rs_database_connect.connect_timeout_ms=10000
rs_database_connect.call_timeout_ms=0
rs_database_connect.session_reset=1
rs_database_connect.parse_cache_entries=512
rs_database_connect.parse_cache_max_bytes=4194304
rs_database_connect.parse_cache_max_sql_bytes=65536
rs_database_connect.lob_max_bytes=16777216
rs_database_connect.stmt_cache_size=20
EOF
  echo "Wrote ${INI_DIR}/rs_database_connect.ini"
fi

echo "Installed ${EXT_DIR}/rs_database_connect.so"
php -m | grep -i rs_database_connect || true
