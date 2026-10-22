--TEST--
INI defaults are readable
--SKIPIF--
<?php
if (!extension_loaded('rs_database_connect')) {
    die('skip rs_database_connect not loaded');
}
?>
--FILE--
<?php
var_dump(ini_get('rs_database_connect.max_connections_per_process'));
var_dump(ini_get('rs_database_connect.lob_max_bytes'));
var_dump(ini_get('rs_database_connect.stmt_cache_size'));
$stats = RsDataBaseConnect::getParseCacheStats();
var_dump(array_key_exists('hits', $stats));
?>
--EXPECT--
string(1) "2"
string(8) "16777216"
string(2) "20"
bool(true)
