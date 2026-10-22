<?php
/**
 * Smoke test against a live PostgreSQL (CI / local).
 *
 * Env: RSDB_DSN (required), extension loaded via -d extension=…
 */
declare(strict_types=1);

$dsn = getenv('RSDB_DSN') ?: 'pg://rsdb:rsdb@127.0.0.1:5432/rsdb_test';

if (!extension_loaded('rs_database_connect')) {
    fwrite(STDERR, "extension not loaded\n");
    exit(1);
}

assert(RsDataBaseConnect::isSupported('pg'));

$db = RsDataBaseConnect::getInstance($dsn, true);
assert($db->isConnection());
assert($db->getDriver() === 'pg');
assert(str_contains($db->getAddress(), '***'));

$same = RsDataBaseConnect::getInstance($dsn, true);
assert($same === $db);

$db->beginTransaction();

$ddl = $db->prepare('CREATE TABLE IF NOT EXISTS smoke_org (id serial PRIMARY KEY, name text, active boolean)');
assert($ddl->execute(), $ddl->getErrorMessage() ?? 'ddl failed');

$ins = $db->prepare(
    'INSERT INTO smoke_org (name, active) VALUES (:name, :active) RETURNING id INTO :newId'
);
$newId = null;
$ins->bindValue('name', 'Acme', -1, RsDataBasePrepare::STRING);
$ins->bindValue('active', true, -1, RsDataBasePrepare::BOOLEAN);
$ins->bind('newId', $newId, -1, RsDataBasePrepare::INTEGER);
assert($ins->execute(), sprintf('[%s] %s', $ins->getErrorCode(), $ins->getErrorMessage()));
assert(is_int($newId) && $newId > 0, 'RETURNING INTO did not set newId');

$sel = $db->prepare('SELECT id, name, active FROM smoke_org WHERE id = :id');
$sel->bindValue('id', $newId, -1, RsDataBasePrepare::INTEGER);
assert($sel->execute());
$row = $sel->fetchRow();
assert(is_array($row));
assert(($row['ID'] ?? $row['id'] ?? null) == $newId);
assert(($row['NAME'] ?? $row['name'] ?? null) === 'Acme');

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
assert($blk->execute(), $blk->getErrorMessage() ?? 'block failed');
assert($resultMessage === 'ДОКУМЕНТ');
assert($resultLength === 8);

// COLLECTION D_CL_ID → bigint[]
$arr = $db->prepare('SELECT :ids::bigint[] AS ids');
$arr->bindValue('ids', [10, 20, 30], -1, RsDataBasePrepare::COLLECTION, 'D_CL_ID');
assert($arr->execute(), $arr->getErrorMessage() ?? 'collection failed');
$arow = $arr->fetchRow();
assert(is_array($arow));
$ids = $arow['IDS'] ?? $arow['ids'] ?? null;
assert(is_array($ids) && count($ids) === 3, 'collection result');

// CLOB as text
$clob = $db->prepare('SELECT length(:body) AS n');
$clob->bindValue('body', str_repeat('я', 100), -1, RsDataBasePrepare::CLOB);
assert($clob->execute(), $clob->getErrorMessage() ?? 'clob failed');
$crow = $clob->fetchRow();
assert(($crow['N'] ?? $crow['n'] ?? null) == 100);

// statement_rollback keeps tx usable after error
$bad = $db->prepare('SELECT * FROM smoke_org_missing_table_xyz');
$okBad = $bad->execute();
assert($okBad === false);
$ok2 = $db->prepare('SELECT 1 AS x');
assert($ok2->execute(), 'tx should still work with statement_rollback=on');

$db->commit();
$db->close();

$stats = RsDataBaseConnect::getParseCacheStats();
assert(($stats['hits'] + $stats['misses']) > 0);

fwrite(STDOUT, "smoke_pg OK\n");
exit(0);
