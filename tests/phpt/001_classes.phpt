--TEST--
Classes, constants and isSupported
--SKIPIF--
<?php
if (!extension_loaded('rs_database_connect')) {
    die('skip rs_database_connect not loaded');
}
?>
--FILE--
<?php
var_dump(class_exists('RsDataBaseConnect'));
var_dump(class_exists('RsDataBasePrepare'));
var_dump(class_exists('RsDataBaseBind'));
var_dump(class_exists('RsDataBaseCursor'));
var_dump(is_subclass_of('RsDataBaseConnectionException', 'RsDataBaseException'));
var_dump(RsDataBasePrepare::INTEGER);
var_dump(RsDataBasePrepare::FLOAT);
var_dump(RsDataBasePrepare::BOOLEAN);
var_dump(RsDataBaseConnect::isSupported('pg'));
var_dump(RsDataBaseConnect::isSupported(null));
$r = new ReflectionClass('RsDataBaseConnect');
var_dump($r->isFinal());
?>
--EXPECT--
bool(true)
bool(true)
bool(true)
bool(true)
bool(true)
string(7) "integer"
string(5) "float"
string(7) "boolean"
bool(true)
bool(true)
bool(true)
