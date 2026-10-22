<?php
/**
 * Smoke test against a live PostgreSQL (CI / local).
 *
 * Env: RSDB_DSN (required), extension loaded via -d extension=…
 *
 * Do not put side effects inside assert(): on CI zend.assertions=-1 skips them.
 */
declare(strict_types=1);

function fail(string $msg): never
{
    fwrite(STDERR, $msg . "\n");
    exit(1);
}

function must_true(bool $ok, string $msg): void
{
    if (!$ok) {
        fail($msg);
    }
}

$dsn = getenv('RSDB_DSN') ?: 'pg://rsdb:rsdb@127.0.0.1:5432/rsdb_test';

if (!extension_loaded('rs_database_connect')) {
    fail('extension not loaded');
}

must_true(RsDataBaseConnect::isSupported('pg'), 'pg driver not supported');

$db = RsDataBaseConnect::getInstance($dsn, true);
must_true($db->isConnection(), 'not connected');
must_true($db->getDriver() === 'pg', 'driver != pg');
must_true(str_contains($db->getAddress(), '***'), 'password not masked');

$same = RsDataBaseConnect::getInstance($dsn, true);
must_true($same === $db, 'getInstance must return same object');

$db->beginTransaction();

$ddl = $db->prepare('CREATE TABLE IF NOT EXISTS smoke_org (id serial PRIMARY KEY, name text, active boolean)');
must_true($ddl->execute(), $ddl->getErrorMessage() ?? 'ddl failed');

$ins = $db->prepare(
    'INSERT INTO smoke_org (name, active) VALUES (:name, :active) RETURNING id INTO :newId'
);
$newId = null;
$ins->bindValue('name', 'Acme', -1, RsDataBasePrepare::STRING);
$ins->bindValue('active', true, -1, RsDataBasePrepare::BOOLEAN);
$ins->bind('newId', $newId, -1, RsDataBasePrepare::INTEGER);
must_true(
    $ins->execute(),
    sprintf('[%s] %s', (string) $ins->getErrorCode(), (string) $ins->getErrorMessage())
);
must_true(is_int($newId) && $newId > 0, 'RETURNING INTO did not set newId');

$sel = $db->prepare('SELECT id, name, active FROM smoke_org WHERE id = :id');
$sel->bindValue('id', $newId, -1, RsDataBasePrepare::INTEGER);
must_true(
    $sel->execute(),
    sprintf('select failed: [%s] %s', (string) $sel->getErrorCode(), (string) $sel->getErrorMessage())
);
$row = $sel->fetchRow();
must_true(is_array($row), 'fetchRow expected array');
must_true(($row['ID'] ?? $row['id'] ?? null) == $newId, 'id mismatch');
must_true(($row['NAME'] ?? $row['name'] ?? null) === 'Acme', 'name mismatch');

$blk = $db->prepare(<<<'SQL'
BEGIN
    :resultMessage := upper(:sourceMessage);
    :resultLength := length(:resultMessage);
END;
SQL);
$blk->bindValue('sourceMessage', 'документ', 4000, RsDataBasePrepare::STRING);
$resultMessage = null;
$resultLength = null;
$blk->bind('resultMessage', $resultMessage, 4000, RsDataBasePrepare::STRING)
    ->bind('resultLength', $resultLength, -1, RsDataBasePrepare::INTEGER);
must_true($blk->execute(), $blk->getErrorMessage() ?? 'block failed');
must_true($resultMessage === 'ДОКУМЕНТ', 'OUT resultMessage mismatch');
must_true($resultLength === 8, 'OUT resultLength mismatch');

$arr = $db->prepare('SELECT :ids::bigint[] AS ids');
$arr->bindValue('ids', [10, 20, 30], -1, RsDataBasePrepare::COLLECTION, 'D_CL_ID');
must_true($arr->execute(), $arr->getErrorMessage() ?? 'collection failed');
$arow = $arr->fetchRow();
must_true(is_array($arow), 'collection fetchRow');
$ids = $arow['IDS'] ?? $arow['ids'] ?? null;
must_true(is_array($ids) && count($ids) === 3, 'collection result');

$clob = $db->prepare('SELECT length(:body) AS n');
$clob->bindValue('body', str_repeat('я', 100), -1, RsDataBasePrepare::CLOB);
must_true($clob->execute(), $clob->getErrorMessage() ?? 'clob failed');
$crow = $clob->fetchRow();
must_true(($crow['N'] ?? $crow['n'] ?? null) == 100, 'clob length');

$bad = $db->prepare('SELECT * FROM smoke_org_missing_table_xyz');
$okBad = $bad->execute();
must_true($okBad === false, 'expected failing execute');
$ok2 = $db->prepare('SELECT 1 AS x');
must_true($ok2->execute(), 'tx should still work with statement_rollback=on');

$db->commit();
$db->close();

$stats = RsDataBaseConnect::getParseCacheStats();
must_true(($stats['hits'] + $stats['misses']) > 0, 'parse cache unused');

fwrite(STDOUT, "smoke_pg OK\n");
exit(0);
