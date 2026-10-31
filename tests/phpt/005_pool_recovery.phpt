--TEST--
Pool slot reuse, transparent reconnect after server kill, no broken-slot leak
--SKIPIF--
<?php
if (!extension_loaded('rs_database_connect')) {
    die('skip rs_database_connect not loaded');
}
if (getenv('RSDB_DSN') === false || getenv('RSDB_DSN') === '') {
    die('skip RSDB_DSN not set');
}
if (PHP_OS_FAMILY === 'Windows' || !exec('command -v psql 2>/dev/null')) {
    die('skip psql not available');
}
if (!str_starts_with(getenv('RSDB_DSN'), 'pg://')) {
    die('skip PostgreSQL DSN required');
}
?>
--FILE--
<?php
$dsn = getenv('RSDB_DSN');

function status_count(string $st): int {
    $n = 0;
    foreach (RsDataBaseConnect::getConnections() as $c) {
        if ($c['status'] === $st) { $n++; }
    }
    return $n;
}
function fail(string $m): never {
    fwrite(STDERR, "FAIL: $m\n");
    exit(1);
}

// 1) release returns the slot as free; the next lease reuses the SAME backend.
$a = RsDataBaseConnect::getInstance($dsn, true);
if ($a->ping() !== true) { fail('ping on fresh connection'); }
$q = $a->prepare('SELECT pg_backend_pid() AS pid');
$q->execute();
$pid1 = (int)$q->fetchRow()['PID'];
$a->close();
if (status_count('free') !== 1) { fail('slot not free after close'); }

$b = RsDataBaseConnect::getInstance($dsn, true);
$q = $b->prepare('SELECT pg_backend_pid() AS pid');
$q->execute();
$pid2 = (int)$q->fetchRow()['PID'];
if ($pid1 !== $pid2) { fail("slot reuse expected backend $pid1, got $pid2"); }
if (RsDataBaseConnect::getConnectionCount() !== 1) { fail('pool grew instead of reusing'); }

// 2) server kills the backend: ping()=false, rollBack() throws, next
//    getInstance() transparently reconnects; the dead slot must be REMOVED.
exec('psql ' . escapeshellarg(parse_dsn_host_db($dsn)) . ' -c ' .
     escapeshellarg("SELECT pg_terminate_backend($pid2)") . ' >/dev/null 2>&1');
usleep(300000);
if ($b->ping() !== false) { fail('ping must fail after pg_terminate_backend'); }
try {
    $b->rollBack();
} catch (RsDataBaseConnectionException $e) {
    // expected: the physical rollback runs on a dead socket
}
$c = RsDataBaseConnect::getInstance($dsn, true);
if (!$c->isConnection() || $c->ping() !== true) { fail('transparent reconnect'); }
if (RsDataBaseConnect::getConnectionCount() !== 1) {
    fail('broken slot leaked: ' . RsDataBaseConnect::getConnectionCount());
}
foreach (RsDataBaseConnect::getConnections() as $s) {
    if ($s['status'] === 'broken') { fail('broken slot lingers in the pool'); }
}

// 3) execute(reconnect: true) recovers within the same statement.
$q = $c->prepare('SELECT 1 AS x');
if ($q->execute(true) !== true) { fail('execute(reconnect)'); }
if ($c->ping() !== true) { fail('ping after execute(reconnect)'); }

// 4) repeated lease cycles must not grow the pool (second concurrent lease is
//    legitimate: max_connections_per_process=2).
$c->close();
for ($i = 0; $i < 4; $i++) {
    $x = RsDataBaseConnect::getNewInstance($dsn, true);
    $x->close();
}
if (RsDataBaseConnect::getConnectionCount() > 2) {
    fail('pool grew unbounded: ' . RsDataBaseConnect::getConnectionCount());
}

// 5) password never leaks into diagnostics.
if (str_contains($c->getAddress(), '@') && !str_contains($c->getAddress(), '***')) {
    fail('password not masked');
}
$c->close();
echo "ok\n";

function parse_dsn_host_db(string $dsn): string {
    // pg://user:pass@host:port/db -> "postgresql://user:pass@host:port/db" (psql accepts it)
    return 'postgresql://' . substr($dsn, strlen('pg://'));
}
?>
--EXPECT--
ok
