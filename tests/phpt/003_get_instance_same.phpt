--TEST--
getInstance returns same object for same DSN
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
$a = RsDataBaseConnect::getInstance($dsn, false);
$b = RsDataBaseConnect::getInstance($dsn, false);
var_dump($a === $b);
var_dump($a->inTransaction());
$a->beginTransaction();
var_dump($b->getTransactionLevel() === 1);
$a->rollBack();
$a->close();
var_dump($a->isConnection());
$c = RsDataBaseConnect::getInstance($dsn, false);
var_dump($c !== $a);
$c->close();
echo "ok\n";
?>
--EXPECT--
bool(true)
bool(false)
bool(true)
bool(false)
bool(true)
ok
