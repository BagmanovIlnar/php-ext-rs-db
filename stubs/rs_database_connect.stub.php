<?php

/**
 * Stub file for IDE autocomplete of the `rs_database_connect` PHP extension.
 *
 * @generated Keep in sync with Rust `#[php_impl]` methods.
 */

/**
 * Base exception for all `rs_database_connect` errors.
 *
 * Thrown for API misuse, SQL/driver failures that are not connection-specific,
 * and internal panics converted via `catch_unwind`.
 *
 * @example
 * ```php
 * try {
 *     $db = RsDataBaseConnect::getInstance('pg://u:p@127.0.0.1:5432/db');
 * } catch (RsDataBaseException $e) {
 *     echo $e->getMessage();
 * }
 * ```
 */
class RsDataBaseException extends Exception
{
}

/**
 * Connection failed, closed, broken, or lease is stale.
 *
 * @example
 * ```php
 * try {
 *     $db->ping();
 * } catch (RsDataBaseConnectionException $e) {
 *     $db->close();
 * }
 * ```
 */
final class RsDataBaseConnectionException extends RsDataBaseException
{
}

/**
 * Persistent pool has no free slot and cannot grow (NTS: no wait).
 *
 * @example
 * ```php
 * try {
 *     $extra = RsDataBaseConnect::getNewInstance($dsn, true);
 * } catch (RsDataBasePoolExhaustedException $e) {
 *     // raise rs_database_connect.max_connections_per_process
 * }
 * ```
 */
final class RsDataBasePoolExhaustedException extends RsDataBaseException
{
}

/**
 * Timed out waiting for a free pool slot (ZTS builds only).
 */
final class RsDataBasePoolTimeoutException extends RsDataBaseException
{
}

/**
 * Lease of a physical Oracle/PostgreSQL connection: pool, transactions, prepare.
 *
 * Constructor is closed. Obtain instances via `getInstance()` / `getNewInstance()`.
 *
 * Within one PHP request, `getInstance($address)` returns the **same** object for
 * the same canonical DSN (`===`). Persistent leases return to the process pool on
 * `close()` / `RSHUTDOWN`; non-persistent connections are closed physically.
 *
 * @example
 * ```php
 * $db = RsDataBaseConnect::getInstance('pg://DEV:secret@127.0.0.1:5432/DEV', true);
 * $db->beginTransaction();
 * $q = $db->prepare('SELECT id FROM t WHERE active = :active');
 * $q->bindValue('active', true, -1, RsDataBasePrepare::BOOLEAN);
 * if (!$q->execute()) {
 *     $db->rollBack();
 *     throw new RsDataBaseException($q->getErrorMessage());
 * }
 * $rows = $q->fetchAllRows();
 * $db->commit();
 * $db->close();
 * ```
 */
final class RsDataBaseConnect
{
    private function __construct()
    {
    }

    /**
     * Return the request-scoped connection for `$address` (same object on repeat).
     *
     * On first call acquires a lease (pool slot when `$persistent`, else a new
     * physical connection). Repeat calls with an equivalent canonical DSN return
     * the same PHP object. A differing `$persistent` on repeat emits `E_WARNING`
     * and keeps the first flag.
     *
     * @param string $address DSN (`pg://…` or `oci8://…`)
     * @param bool $persistent Use the process-local persistent pool
     * @return RsDataBaseConnect
     *
     * @throws RsDataBaseConnectionException
     * @throws RsDataBasePoolExhaustedException
     * @throws RsDataBasePoolTimeoutException
     *
     * @example
     * ```php
     * $a = RsDataBaseConnect::getInstance('pg://u:p@h:5432/db', true);
     * $b = RsDataBaseConnect::getInstance('pg://u:p@h:5432/db', true);
     * var_dump($a === $b); // true
     * ```
     */
    public static function getInstance(string $address, bool $persistent = false): RsDataBaseConnect
    {
    }

    /**
     * Independent lease to the same address (separate transaction).
     *
     * Not registered in the getInstance() map. When `$persistent` is true, takes
     * another pool slot (subject to `max_connections_per_process`).
     *
     * @param string $address
     * @param bool $persistent
     * @return RsDataBaseConnect
     *
     * @throws RsDataBaseConnectionException
     * @throws RsDataBasePoolExhaustedException
     *
     * @example
     * ```php
     * $main = RsDataBaseConnect::getInstance($dsn, true);
     * $audit = RsDataBaseConnect::getNewInstance($dsn, true);
     * $audit->beginTransaction();
     * // … commit audit even if $main rolls back
     * ```
     */
    public static function getNewInstance(string $address, bool $persistent = false): RsDataBaseConnect
    {
    }

    /**
     * Snapshot of connections in this PHP process (password masked as `***`).
     *
     * @return list<array{address: string, status: string, persistent: bool, driver: string, idle_ms?: int}>
     *
     * @example
     * ```php
     * foreach (RsDataBaseConnect::getConnections() as $row) {
     *     echo $row['address'], ' ', $row['status'], PHP_EOL;
     * }
     * ```
     */
    public static function getConnections(): array
    {
    }

    /**
     * Number of connections tracked in this process (busy + free + detached).
     *
     * @return int
     */
    public static function getConnectionCount(): int
    {
    }

    /**
     * Whether the compiled extension supports a DSN scheme (`oci8`, `pg`) or any.
     *
     * @param string|null $scheme `oci8`, `pg`, or null for “any driver built in”
     * @return bool
     *
     * @example
     * ```php
     * if (!RsDataBaseConnect::isSupported('pg')) {
     *     throw new RuntimeException('PostgreSQL driver not compiled');
     * }
     * ```
     */
    public static function isSupported(?string $scheme = null): bool
    {
    }

    /**
     * Process-global SQL parse-cache statistics.
     *
     * @return array{entries: int, bytes: int, hits: int, misses: int, evictions: int}
     */
    public static function getParseCacheStats(): array
    {
    }

    /**
     * Clear the process-global SQL parse cache.
     *
     * @return void
     */
    public static function clearParseCache(): void
    {
    }

    /**
     * Whether the lease is open and usable.
     *
     * @return bool
     */
    public function isConnection(): bool
    {
    }

    /**
     * Masked DSN (`password` shown as `***`).
     *
     * @return string
     */
    public function getAddress(): string
    {
    }

    /**
     * Driver kind: `oci8` or `pg`.
     *
     * @return string
     */
    public function getDriver(): string
    {
    }

    /**
     * Whether this lease was acquired as persistent.
     *
     * @return bool
     */
    public function isPersistent(): bool
    {
    }

    /**
     * Liveness check (Oracle ping / PostgreSQL trivial query).
     *
     * @return bool
     * @throws RsDataBaseConnectionException
     */
    public function ping(): bool
    {
    }

    /**
     * Create a statement object for `$sql` (parse is cached by driver+text).
     *
     * @param string $sql
     * @return RsDataBasePrepare
     * @throws RsDataBaseException
     * @throws RsDataBaseConnectionException
     */
    public function prepare(string $sql): RsDataBasePrepare
    {
    }

    /**
     * Begin or nest a transaction (level ≥ 2 → SAVEPOINT).
     *
     * @return RsDataBaseConnect
     * @throws RsDataBaseException
     * @throws RsDataBaseConnectionException
     */
    public function beginTransaction(): RsDataBaseConnect
    {
    }

    /**
     * Commit outermost level or RELEASE SAVEPOINT for nested levels.
     *
     * @return RsDataBaseConnect
     * @throws RsDataBaseException
     * @throws RsDataBaseConnectionException
     */
    public function commit(): RsDataBaseConnect
    {
    }

    /**
     * Full ROLLBACK or ROLLBACK TO SAVEPOINT for nested levels.
     *
     * @return RsDataBaseConnect
     * @throws RsDataBaseException
     * @throws RsDataBaseConnectionException
     */
    public function rollBack(): RsDataBaseConnect
    {
    }

    /**
     * True when `getTransactionLevel() > 0`.
     *
     * @return bool
     */
    public function inTransaction(): bool
    {
    }

    /**
     * Explicit transaction nesting depth.
     *
     * @return int
     */
    public function getTransactionLevel(): int
    {
    }

    /**
     * Release the lease (rollback; return persistent slot or close connection).
     *
     * Idempotent. After close, `isConnection()` is false and prepares are invalid.
     *
     * @return void
     */
    public function close(): void
    {
    }

    /**
     * Last driver error code (`ORA-…` or SQLSTATE), or null.
     *
     * @return string|null
     */
    public function getErrorCode(): ?string
    {
    }

    /**
     * Last driver/API error message (password never included), or null.
     *
     * @return string|null
     */
    public function getErrorMessage(): ?string
    {
    }
}

/**
 * Prepared statement: bind parameters, execute, fetch rows.
 *
 * Created only via {@see RsDataBaseConnect::prepare()}.
 *
 * `execute()` returns `true` on success and `false` on driver errors (code/message
 * via getters). API misuse and closed/broken leases throw.
 *
 * @example
 * ```php
 * $q = $db->prepare('SELECT id, name FROM organisation WHERE id = :id');
 * $q->bindValue('id', 42, -1, RsDataBasePrepare::INTEGER);
 * if (!$q->execute()) {
 *     throw new RsDataBaseException(sprintf('[%s] %s', $q->getErrorCode(), $q->getErrorMessage()));
 * }
 * while ($row = $q->fetchRow()) {
 *     echo $row['ID'], ' ', $row['NAME'], PHP_EOL;
 * }
 * ```
 */
final class RsDataBasePrepare
{
    public const CURSOR = 'cursor';
    public const COLLECTION = 'collection';
    public const ARRAY = 'array';
    public const CLOB = 'clob';
    public const BLOB = 'blob';
    public const INTEGER = 'integer';
    public const FLOAT = 'float';
    public const STRING = 'string';
    public const BOOLEAN = 'boolean';
    public const NULL = 'null';

    /**
     * Create a detached bind object attached to this statement.
     *
     * @return RsDataBaseBind
     *
     * @example
     * ```php
     * $id = 1;
     * $q->createBind()->setName('id')->setValue($id)->setType(RsDataBasePrepare::INTEGER);
     * ```
     */
    public function createBind(): RsDataBaseBind
    {
    }

    /**
     * Bind IN/OUT by reference (`$value` updated after successful execute for OUT).
     *
     * Names `id` and `:id` are equivalent. `$type = null` enables auto-detection.
     * For `CURSOR`, `$value` becomes an `RsDataBaseCursor` after a successful execute.
     * For `COLLECTION`, pass `$tdo` (`D_CL_ID` / `D_CL_STR` on PostgreSQL).
     *
     * @param string $variableName
     * @param mixed $value
     * @param int $size
     * @param string|null $type One of the class constants
     * @param string|null $tdo Oracle/PG type descriptor for COLLECTION
     * @return RsDataBasePrepare
     * @throws RsDataBaseException
     *
     * @example
     * ```php
     * $cur = null;
     * $q->bind('rc', $cur, -1, RsDataBasePrepare::CURSOR);
     * $q->execute();
     * $rows = $cur->fetchAllRows();
     *
     * $ids = [10, 20];
     * $q->bindValue('ids', $ids, -1, RsDataBasePrepare::COLLECTION, 'D_CL_ID');
     * ```
     */
    public function bind(
        string $variableName,
        mixed &$value,
        int $size = -1,
        ?string $type = null,
        ?string $tdo = null
    ): RsDataBasePrepare {
    }

    /**
     * Bind IN with an owned copy of `$value` (not by reference).
     *
     * @param string $variableName
     * @param mixed $value
     * @param int $size
     * @param string|null $type
     * @param string|null $tdo
     * @return RsDataBasePrepare
     * @throws RsDataBaseException
     */
    public function bindValue(
        string $variableName,
        mixed $value,
        int $size = -1,
        ?string $type = null,
        ?string $tdo = null
    ): RsDataBasePrepare {
    }

    /**
     * Detach a bind by name (`id` / `:id`) or by {@see RsDataBaseBind} object.
     *
     * @param string|RsDataBaseBind $bind
     * @return bool True if a bind was removed
     */
    public function removeBind(string|RsDataBaseBind $bind): bool
    {
    }

    /**
     * Bound parameters in creation order.
     *
     * @return list<RsDataBaseBind>
     */
    public function getBinds(): array
    {
    }

    /**
     * Original SQL text passed to `prepare()`.
     *
     * @return string|null
     */
    public function getSql(): ?string
    {
    }

    /**
     * Whether the statement is an anonymous PL/SQL / PL/pgSQL block.
     *
     * @return bool
     */
    public function isPLSQL(): bool
    {
    }

    /**
     * Execute with current binds.
     *
     * @param bool $reconnect Allow reconnect only when no physical transaction and lease not broken
     * @return bool `true` on success, `false` on driver error
     * @throws RsDataBaseException
     * @throws RsDataBaseConnectionException
     */
    public function execute(bool $reconnect = false): bool
    {
    }

    /**
     * Fetch next associative row, or `null` when exhausted.
     *
     * Column key case follows DSN `column_case` (default `upper`).
     *
     * @return array<string, mixed>|null
     * @throws RsDataBaseException
     */
    public function fetchRow(): ?array
    {
    }

    /**
     * Fetch up to `$count` rows.
     *
     * @param int $count
     * @return list<array<string, mixed>>
     * @throws RsDataBaseException
     */
    public function fetchRows(int $count): array
    {
    }

    /**
     * Fetch the entire result into memory.
     *
     * @return list<array<string, mixed>>
     * @throws RsDataBaseException
     */
    public function fetchAllRows(): array
    {
    }

    /**
     * Rows affected by the last DML (or 0 for SELECT).
     *
     * @return int
     */
    public function getAffectedRows(): int
    {
    }

    /**
     * Number of columns in the current result, or 0.
     *
     * @return int
     */
    public function getColumnCount(): int
    {
    }

    /**
     * Parent connection object.
     *
     * @return RsDataBaseConnect
     */
    public function getConnection(): RsDataBaseConnect
    {
    }

    /**
     * Free statement resources (not the connection).
     *
     * @return void
     */
    public function free(): void
    {
    }

    /** @return string|null */
    public function getErrorCode(): ?string
    {
    }

    /** @return string|null */
    public function getErrorMessage(): ?string
    {
    }

    /** Enable per-statement execution log. @return void */
    public function enableLog(): void
    {
    }

    /** Disable per-statement execution log. @return void */
    public function disableLog(): void
    {
    }

    /**
     * Collected log entries when logging is enabled.
     *
     * @return list<array<string, mixed>>
     */
    public function getLog(): array
    {
    }
}

/**
 * One bind parameter, created via {@see RsDataBasePrepare::createBind()}.
 *
 * @example
 * ```php
 * $msg = null;
 * $q->createBind()
 *     ->setName('resultMessage')
 *     ->setValue($msg)
 *     ->setSize(4000)
 *     ->setType(RsDataBasePrepare::STRING);
 * ```
 */
final class RsDataBaseBind
{
    /**
     * Set placeholder name (`id` and `:id` are equivalent).
     *
     * @param string $name
     * @return RsDataBaseBind
     */
    public function setName(string $name): RsDataBaseBind
    {
    }

    /**
     * Bind PHP variable by reference for IN/OUT.
     *
     * @param mixed $value
     * @return RsDataBaseBind
     */
    public function setValue(mixed &$value): RsDataBaseBind
    {
    }

    /**
     * Buffer size hint (−1 = driver default).
     *
     * @param int $size
     * @return RsDataBaseBind
     */
    public function setSize(int $size = -1): RsDataBaseBind
    {
    }

    /**
     * Explicit type or `null` for auto-detection.
     *
     * @param string|null $type
     * @return RsDataBaseBind
     */
    public function setType(?string $type): RsDataBaseBind
    {
    }

    /**
     * Oracle type descriptor object name for COLLECTION.
     *
     * @param string|null $tdo
     * @return RsDataBaseBind
     */
    public function setTdo(?string $tdo): RsDataBaseBind
    {
    }
}

/**
 * Result of an Oracle OUT REF CURSOR / PostgreSQL refcursor (stage 2).
 *
 * @example
 * ```php
 * $cursor = null;
 * $q->bind('resultCursor', $cursor, -1, RsDataBasePrepare::CURSOR);
 * $q->execute();
 * /** @var RsDataBaseCursor $cursor *\/
 * $rows = $cursor->fetchAllRows();
 * ```
 */
final class RsDataBaseCursor
{
    /**
     * @return array<string, mixed>|null
     * @throws RsDataBaseException
     */
    public function fetchRow(): ?array
    {
    }

    /**
     * @param int $count
     * @return list<array<string, mixed>>
     * @throws RsDataBaseException
     */
    public function fetchRows(int $count): array
    {
    }

    /**
     * @return list<array<string, mixed>>
     * @throws RsDataBaseException
     */
    public function fetchAllRows(): array
    {
    }

    /** @return int */
    public function getColumnCount(): int
    {
    }

    /** @return void */
    public function free(): void
    {
    }
}
