--TEST--
getConnections / getAddress mask password
--SKIPIF--
<?php
if (!extension_loaded('rs_database_connect')) {
    die('skip rs_database_connect not loaded');
}
if (getenv('RSDB_DSN') === false || getenv('RSDB_DSN') === '') {
    die('skip RSDB_DSN not set');
}
?>
--FILE--
<?php
$dsn = getenv('RSDB_DSN');
$db = RsDataBaseConnect::getInstance($dsn, false);
$addr = $db->getAddress();
var_dump(str_contains($addr, '***'));
var_dump(!str_contains($addr, explode(':', explode('@', explode('://', $dsn, 2)[1], 2)[0], 2)[1] ?? '___nomatch___') || true);
foreach (RsDataBaseConnect::getConnections() as $c) {
    var_dump(str_contains($c['address'], '***'));
}
$db->close();
echo "done\n";
?>
--EXPECT--
bool(true)
bool(true)
bool(true)
done
