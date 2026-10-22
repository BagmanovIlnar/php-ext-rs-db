<?php
/**
 * Oracle smoke (requires Instant Client + RSDB_ORA_DSN).
 *
 * Example: oci8://user:pass@host:1521/service:dedicated
 *
 * Do not put side effects inside assert(): on CI zend.assertions=-1 skips them.
 */
declare(strict_types=1);

function fail(string $msg): never
{
    fwrite(STDERR, $msg . "\n");
    exit(1);
}

$dsn = getenv('RSDB_ORA_DSN');
if ($dsn === false || $dsn === '') {
    fwrite(STDERR, "skip: RSDB_ORA_DSN not set\n");
    exit(0);
}

if (!extension_loaded('rs_database_connect')) {
    fail('extension not loaded');
}

if (!RsDataBaseConnect::isSupported('oci8')) {
    fail('Oracle driver not compiled (build with --features oracle)');
}

$db = RsDataBaseConnect::getInstance($dsn, true);
if (!$db->isConnection() || $db->getDriver() !== 'oci8') {
    fail('oracle connect failed');
}
if (!str_contains($db->getAddress(), '***')) {
    fail('password not masked');
}

$db->beginTransaction();
$q = $db->prepare('SELECT :n + 1 AS n FROM dual');
$q->bindValue('n', 41, -1, RsDataBasePrepare::INTEGER);
if (!$q->execute()) {
    fail($q->getErrorMessage() ?? 'select failed');
}
$row = $q->fetchRow();
if (!is_array($row) || ($row['N'] ?? null) != 42) {
    fail('unexpected dual result');
}

$db->commit();
$db->close();
fwrite(STDOUT, "smoke_oracle OK\n");
exit(0);
