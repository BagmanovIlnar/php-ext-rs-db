<?php
/**
 * Oracle smoke (requires Instant Client + RSDB_ORA_DSN).
 *
 * Example: oci8://user:pass@host:1521/service:dedicated
 */
declare(strict_types=1);

$dsn = getenv('RSDB_ORA_DSN');
if ($dsn === false || $dsn === '') {
    fwrite(STDERR, "skip: RSDB_ORA_DSN not set\n");
    exit(0);
}

if (!extension_loaded('rs_database_connect')) {
    fwrite(STDERR, "extension not loaded\n");
    exit(1);
}

if (!RsDataBaseConnect::isSupported('oci8')) {
    fwrite(STDERR, "Oracle driver not compiled (build with --features oracle)\n");
    exit(1);
}

$db = RsDataBaseConnect::getInstance($dsn, true);
assert($db->isConnection());
assert($db->getDriver() === 'oci8');
assert(str_contains($db->getAddress(), '***'));

$db->beginTransaction();
$q = $db->prepare('SELECT :n + 1 AS n FROM dual');
$q->bindValue('n', 41, -1, RsDataBasePrepare::INTEGER);
assert($q->execute(), $q->getErrorMessage() ?? 'select failed');
$row = $q->fetchRow();
assert(is_array($row));
assert(($row['N'] ?? null) == 42);

$db->commit();
$db->close();
fwrite(STDOUT, "smoke_oracle OK\n");
exit(0);
