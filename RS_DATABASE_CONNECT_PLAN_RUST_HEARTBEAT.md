# План разработки Rust-расширения `rs_database_connect` (редакция 4)

## Цель

Синхронное расширение для PHP 8.2–8.5, работающее с Oracle и PostgreSQL:

- `RsDataBaseConnect` — lease физического соединения, persistent-пул, транзакции.
- `RsDataBasePrepare` — SQL, bind-параметры, выполнение, результат.
- `RsDataBaseBind` — параметр одного bind и значение по ссылке.
- `RsDataBaseCursor` — результат OUT REF CURSOR.
- API не зависит от Kernel-классов, но повторяет возможности `d3/kernel/DataBase/Query`.

## 0. Принципы

1. **Фоновые потоки выключены по умолчанию.** Базовый режим: проверка при выдаче lease + настройки TCP/Oracle Net. Опциональный Oracle DRCP heartbeat разрешается только после подтверждения потокобезопасности выбранного драйвера/ODPI-C, сериализации всех операций на соединении, безопасного завершения потока и интеграционных тестов. Не реализовывать heartbeat наугад.
2. **Пул не хранит ничего, что принадлежит PHP.** Глобальное состояние содержит только owned Rust-значения. Zval, Zend-ссылки и объекты PHP живут не дольше запроса и освобождаются только в PHP-потоке. Единственное исключение — **реестр экземпляров запроса** (раздел 4): он хранит ссылки на PHP-объекты `RsDataBaseConnect`, отделён от процессного пула и полностью очищается в `RSHUTDOWN`.
3. **Соединение, вернувшееся в пул, всегда чистое:** без транзакции, без session-состояния прошлого владельца. Если чистоту гарантировать нельзя, slot закрывается и пересоздаётся.
4. **Поведение драйверов выравнивается по Oracle:** ошибка отдельного statement не «отравляет» транзакцию.
5. **Пароль не попадает** в диагностику, логи, сообщения и (по возможности) stack trace.

## 1. Сборка расширения

- Crate `bin/php/extensions/rs_database_connect/`, тип `cdylib`.
- `ext-php-rs` — актуальная `0.15.x` (на момент плана есть 0.15.6; зафиксировать в `Cargo.lock`). Bindings генерируются под одну версию PHP за сборку, поэтому собирать отдельно под каждую версию в матрице.
- **Поддерживаемые версии PHP: 8.2–8.5, все обязательные.** `ext-php-rs 0.15` при сборке под 8.0 выдаёт предупреждение (версия EOL), и upstream 8.0 больше не тестирует, поэтому проверка 8.0 лежит на нашем CI. Если проявится проблема, специфичная для 8.0, варианты: зафиксировать версию `ext-php-rs`, на которой 8.0 официально тестировался (определить по её changelog), или обойти в нашем коде через `cfg` по версии PHP.
- Код и stub используют только возможности PHP 8.0: union types (`string|RsDataBaseBind`) и `mixed` допустимы; enum, readonly-свойства, `never`, first-class callable — нельзя (8.1+). В сигнатурах явный `?type` вместо неявного nullable (deprecated в 8.4); классы `final`, динамические свойства запрещены (deprecated в 8.2).
- Oracle: crate `oracle` (ODPI-C) + существующий Oracle Instant Client.
- PostgreSQL: синхронный crate `postgres` + TLS-коннектор на OpenSSL (`postgres-openssl`). **Проверить**, что он не создаёт фоновых потоков (он работает на собственном runtime): принцип 1 верен только в этом случае. Синхронный клиент обрабатывает сообщения сервера (в том числе разрыв соединения) лишь во время вызовов, поэтому состояние простаивающего соединения выясняется только проверкой при выдаче.
- Каждый `execute`/вызов расширения обёрнут в `catch_unwind`; panic превращается в `RsDataBaseException`, а не в abort процесса.
- PHP stub с полным PHPDoc и примером для каждого метода.
- Тесты: PHPT, Rust unit, интеграционные.

## 2. DSN и параметры подключения

```text
oci8://user:password@host:port/service:pooled
oci8://user:password@host:port/service:dedicated
pg://user:password@host:port/database
```

- `user` и `password` percent-decoded; IPv6-хост в `[...]`.
- Oracle-режимы `pooled`/`dedicated` регистронезависимы. Только `pooled` включает DRCP (connection class `RS_DATABASE_CONNECT`, purity `SELF`; проверить на реальном стенде).
- Query-параметры DSN (входят в canonical pool key; неизвестные параметры — ошибка):

| Параметр | Драйвер | Назначение |
| --- | --- | --- |
| `connect_timeout_ms` | оба | таймаут подключения (перекрывает INI) |
| `call_timeout_ms` | оба | таймаут вызова: Oracle call timeout, PG `statement_timeout` (стартовый параметр `options`, см. ниже) |
| `keepalive_s` | оба | интервал TCP keepalive: PG `keepalives_idle` (секунды); Oracle easy-connect `expire_time` задаётся **в минутах**: значение округляется вверх до целых минут, минимум 1 (клиент 19c+, проверить) |
| `protocol=tcp\|tcps` | oci8 | TLS для Oracle |
| `sslmode=disable\|prefer\|require\|verify-ca\|verify-full`, `sslrootcert` | pg | TLS для PostgreSQL |
| `application_name` | pg | имя приложения |
| `statement_rollback=on\|off` | pg | неявный savepoint на statement (по умолчанию `on`, см. раздел 4) |
| `column_case=upper\|lower\|native` | оба | регистр ключей результата (по умолчанию `upper`, см. раздел 5) |

- Параметры сессии из DSN/INI (`statement_timeout`, `application_name`) передаются PostgreSQL **стартовыми параметрами** соединения (`options`, `application_name`). Тогда `RESET ALL`/`DISCARD ALL` возвращает их к этим значениям, а не к серверным умолчаниям. Oracle call timeout — атрибут соединения, сброс состояния пакетов его не затрагивает.
- `getAddress()`, `getConnections()`, ошибки и логи показывают DSN с `***` вместо пароля. Query-параметры, содержащие секреты (`sslkey` и т.п.), не поддерживаются.
- Пароль может попасть в stack trace как аргумент `getInstance()`. Документация и `docker/php` ini включают `zend.exception_ignore_args=On` для production; расширение не может это гарантировать.

## 3. Жизненный цикл расширения

**Терминология.** В плане «запрос» без уточнения означает **PHP-запрос**: одно выполнение скрипта в процессе (HTTP-запрос в FPM, весь запуск в CLI), которое заканчивается фазой `RSHUTDOWN`. SQL-запросы называются «SQL» или «statement». В одном PHP-запросе можно выполнить сколько угодно statement'ов и транзакций; все они идут через одно соединение (`getInstance()` возвращает его на каждый вызов). Завершение lease по `RSHUTDOWN` происходит только после того, как скрипт закончил работу, включая `register_shutdown_function` и деструкторы пользовательских объектов: SQL в них ещё выполняется.

| Фаза | Действие |
| --- | --- |
| `MINIT` | Регистрация классов, констант, INI. Валидация значений INI: неверное значение — отказ загрузки с понятным сообщением. |
| `RINIT` | Сброс per-request счётчиков, лога и создание пустого реестра экземпляров запроса. |
| `RSHUTDOWN` | Для каждого lease из реестра экземпляров запроса выполняется завершение lease (раздел 4, «Завершение lease»): persistent — rollback, сброс сессии и возврат slot'а в пул; не persistent — rollback и физическое закрытие соединения. Затем реестр очищается (снимаются ссылки на PHP-объекты). Persistent-соединение при этом **не закрывается**: оно остаётся в пуле процесса для следующего запроса. Все Bind/Prepare текущего запроса инвалидируются. Выполняется даже при fatal error и `exit`. |
| `MSHUTDOWN` | Корректное закрытие всех соединений процессного реестра. |

**fork:** реестр хранит PID создавшего процесса. Если PID изменился (`pcntl_fork`), унаследованные соединения считаются чужими: ими нельзя пользоваться и их нельзя закрывать (иначе будет разорвано соединение родителя). Дескрипторы забываются, пул создаётся заново.

## 4. `RsDataBaseConnect`

Отвечает только за lease физического соединения, пул и транзакции. Конструктор закрытый.

### Фабрика и диагностика

```php
public static function getInstance(string $address, bool $persistent = false): RsDataBaseConnect;

/** @deprecated Использовать getInstance(). Удалить в следующей мажорной версии. */
public static function getInctance(string $address, bool $persistent = false): RsDataBaseConnect;

/** Независимое соединение к тому же адресу: отдельный lease и отдельная транзакция. */
public static function getNewInstance(string $address, bool $persistent = false): RsDataBaseConnect;

public static function getConnections(): array;
public static function getConnectionCount(): int;
public static function isSupported(?string $scheme = null): bool;

public function isConnection(): bool;
public function getAddress(): string;      // всегда с замаскированным паролем
public function getDriver(): string;       // oci8|pg
public function isPersistent(): bool;
public function ping(): bool;
public function prepare(string $sql): RsDataBasePrepare;
```

**Семантика `getInstance()` (подтверждено требованием legacy-кода): в пределах одного запроса одинаковый адрес даёт одно и то же соединение.**

- Реестр экземпляров запроса: `canonical DSN → RsDataBaseConnect`. Ключ — канонический DSN (регистронезависимые режимы, нормализованный percent-decoding, отсортированные query-параметры, пароль), поэтому эквивалентные записи адреса дают один ключ.
- Первый `getInstance($address, …)` берёт lease (slot из пула или непостоянное соединение) и создаёт PHP-объект. Повторные вызовы с тем же адресом возвращают **тот же PHP-объект** (`$a === $b`), нового lease не берут.
- Реестр держит сильную ссылку на объект до `close()` или `RSHUTDOWN`. Поэтому `RsDataBaseConnect::getInstance($addr)->beginTransaction()` … `RsDataBaseConnect::getInstance($addr)->commit()` работают с одним соединением и одной транзакцией, а `unset($db)` lease не освобождает и транзакцию не откатывает. Это соответствует прежнему поведению; вложенные `beginTransaction()` из разных мест кода превращаются в savepoint'ы (раздел 4, «Транзакции»).
- Значение `$persistent` при повторном вызове с тем же адресом не меняет соединение: побеждает флаг первого вызова, расхождение сопровождается `E_WARNING` (решение требует подтверждения, открытый вопрос 9).
- `close()` освобождает lease для всех держателей объекта и удаляет запись из реестра. Объект после этого закрыт (`isConnection() === false`), а следующий `getInstance()` с тем же адресом берёт новый lease и создаёт новый объект.
- Сломанный lease (`broken`) без активной транзакции при следующем `getInstance()` прозрачно заменяется новым. Если транзакция была активна, возвращается тот же объект, все операции бросают `RsDataBaseConnectionException` до `rollBack()`/`close()` (скрытая замена потеряла бы транзакцию).
- Разные адреса дают разные соединения и независимые транзакции.
- Независимое второе соединение к тому же адресу даёт `getNewInstance()` (ниже).
- `getConnections()` и `getConnectionCount()` показывают только соединения текущего процесса.

**Семантика `getNewInstance($address, $persistent = false)`:** всегда новый lease к тому же адресу с отдельной транзакцией (типичный случай: запись в журнал/аудит, которая должна зафиксироваться независимо от основной транзакции).

- Объект **не попадает** в реестр экземпляров запроса: `getInstance()` его не возвращает и им не затрагивается, и наоборот.
- `persistent=true` — берёт ещё один slot из пула процесса, в пределах `max_connections_per_process`; если все slot'ы заняты, в NTS сразу `RsDataBasePoolExhaustedException`. Один `getInstance()` и один `getNewInstance()` на DSN требуют значения не меньше 2 (по умолчанию 2); для нескольких одновременных независимых соединений лимит нужно поднять.
- `persistent=false` — новое физическое соединение вне пула и вне лимита.
- Реестр запроса не держит PHP-объект: когда исчезает последняя ссылка, `__destruct()` равносилен `close()` (rollback; возврат slot'а или закрытие по правилам «Завершение lease»). Реестр запроса хранит только идентификаторы lease (не ссылки на PHP-объекты), поэтому `RSHUTDOWN` освобождает те, что ещё открыты. Закрытие идемпотентно: объект, уничтоженный уже после `RSHUTDOWN`, повторно ничего не делает.
- `getConnections()` и `getConnectionCount()` учитывают оба вида соединений.

`getConnections()` возвращает элементы:

```php
[
    'address'    => 'oci8://user:***@host:1521/service:pooled',
    'status'     => 'busy',     // busy|free
    'persistent' => true,
    'driver'     => 'oci8',
    'idle_ms'    => 1250,       // для free: время простоя
]
```

### Исключения

Все наследуют `RsDataBaseException`:

- `RsDataBaseConnectionException` — ошибка подключения, закрытое/сломанное соединение.
- `RsDataBasePoolExhaustedException` — пул исчерпан, ожидание бессмысленно (NTS).
- `RsDataBasePoolTimeoutException` — истёк `pool_wait_timeout_ms` (ZTS).

### INI (`PHP_INI_SYSTEM`, не меняются через `ini_set()`)

```ini
extension=rs_database_connect.so
rs_database_connect.max_connections_per_process=2
rs_database_connect.pool_wait_timeout_ms=30000
rs_database_connect.idle_validate_ms=60000
rs_database_connect.persistent_timeout_s=0
rs_database_connect.connect_timeout_ms=10000
rs_database_connect.call_timeout_ms=0
rs_database_connect.session_reset=1
rs_database_connect.parse_cache_entries=512
rs_database_connect.parse_cache_max_bytes=4194304
rs_database_connect.parse_cache_max_sql_bytes=65536
```

- `max_connections_per_process` — максимум одновременных соединений к одному DSN **в одном PHP-процессе** (≥ 1), а не общий лимит на все процессы. Рост ленивый: значение — потолок, а не число соединений, которые держатся постоянно (аналог `oci8.max_persistent`, но допускает несколько соединений на DSN).
- `pool_wait_timeout_ms` — только для ZTS (в NTS ожидание невозможно, см. ниже).
- `idle_validate_ms` — если slot простаивал дольше, при выдаче lease он проверяется; `0` отключает проверку (аналог `oci8.ping_interval`).
- `persistent_timeout_s` — slot, простаивающий дольше, закрывается (проверяется при обращении к пулу и в `RSHUTDOWN`); `0` — без ограничения (аналог `oci8.persistent_timeout`). Нужен, чтобы после пика нагрузки workers не держали лишние соединения бесконечно. При смене пароля в DSN создаётся новый пул, а старый остаётся до истечения этого таймаута, поэтому при ротации паролей значение `0` не рекомендуется.
- `connect_timeout_ms`, `call_timeout_ms` — значения по умолчанию, перекрываются параметрами DSN; `call_timeout_ms=0` — без ограничения.
- `session_reset` — сброс сессии при возврате slot'а (см. ниже): `0` — выключен (для PgBouncer в transaction-режиме); `1` — полный (PostgreSQL `DISCARD ALL`, Oracle `RESET_PACKAGE`); `2` — мягкий для PostgreSQL без `DEALLOCATE ALL`, сохраняет prepared statements (для Oracle равен `1`).
- `parse_cache_entries`, `parse_cache_max_bytes`, `parse_cache_max_sql_bytes` — лимиты кеша разбора SQL (раздел 5); `parse_cache_entries=0` отключает кеш. SQL длиннее `parse_cache_max_sql_bytes` не кешируется.

### Модель хранения соединений (как у `oci_pconnect`)

Решение: **общей памяти между процессами нет**, соединения живут внутри процесса. Это та же модель, что у `oci_pconnect`/`pg_pconnect`; общий пул между процессами в PHP невозможен, потому что сокет, TLS-сессию и состояние драйвера нельзя передать другому процессу.

- Реестр соединений — статическая структура внутри `.so`, отдельная в каждом PHP-процессе (FPM worker, CLI).
- Соединение не принадлежит запросу. Скрипт завершился — процесс (worker) продолжает жить и обрабатывает следующий запрос. Persistent-соединение хранится на уровне процесса и в конце запроса не закрывается: `RSHUTDOWN` откатывает транзакцию, сбрасывает сессию и возвращает slot в пул «чистым» (раздел 3). Следующий запрос в этом же worker'е получает уже открытое соединение.
- Новое соединение создаётся только: при первом `getInstance(..., true)` в данном процессе (в том числе в новом worker'е после `pm.max_requests`, reload или crash); после `persistent_timeout_s`; после провала проверки при выдаче; после обрыва соединения БД/сетью.
- Workers не делятся соединениями: worker A не может использовать соединение worker'а B. Пулы изолированы, друг друга они не ждут и не ограничивают.
- В коротком CLI-скрипте пул умирает вместе с процессом, `persistent=true` смысла не имеет. В долгоживущих CLI-процессах (демоны, consumer'ы очередей) persistent полезен, но запрос там один на всю жизнь процесса: `RSHUTDOWN` не наступает, поэтому lease нужно возвращать явным `close()` после каждой единицы работы.
- Режим FPM влияет на время жизни: `static` — workers живут долго, соединения устойчивы; `dynamic` — часть workers создаётся и убирается по нагрузке; `ondemand` — worker, а с ним и соединения, живут недолго.
- Соединение открывается лениво, при первом обращении, а не при старте worker'а.

**Расчёт нагрузки на БД.** Лимит `max_connections_per_process` действует на один процесс. Общее число соединений:

```text
максимум = pm.max_children × max_connections_per_process × число разных DSN
обычно   = число активных workers × 1 × число разных DSN
```

Пример: 10 workers и `max_connections_per_process=5` дают теоретический максимум 50 соединений на DSN, но так как worker обрабатывает один запрос за раз, обычно получается около 10 (по одному на worker). Обычно занят один slot на DSN (`getInstance()`); второй занимает `getNewInstance()` для persistent. Поэтому значение по умолчанию 2 соответствует схеме «один `getInstance()` + один `getNewInstance()`», а при ленивом росте второй slot создаётся только когда он действительно нужен. Если код держит несколько `getNewInstance()` одновременно, лимит нужно поднять.

Максимум должен укладываться в `max_connections` (PostgreSQL) или `processes`/`sessions` (Oracle). Это нужно проверять при настройке; документировать в `docker/php/README.md`.

**Общий лимит на все процессы** и реальное разделение соединений между процессами выполняются вне расширения:
- Oracle — режим `:pooled` (DRCP): общий пул серверных процессов на стороне БД; расширение держит клиентское соединение на worker.
- PostgreSQL — внешний пулер (PgBouncer). В transaction-режиме PgBouncer ломает session-состояние и server-side prepared statements; механизм IN/OUT через транзакционный `set_config` остаётся рабочим, но `session_reset` нужно выставить в `0`.

### Persistent-пул

- Пул отдельный для каждого canonical DSN (включая пароль и query-параметры, хранится как хэш) и для каждого PHP worker/CLI-процесса.
- **Ленивый рост:** `getInstance($address, true)` для адреса, у которого в этом запросе ещё нет lease (иначе возвращается существующий объект), берёт free slot; если свободных нет и `current < max_connections_per_process`, создаётся ровно одно новое соединение. Прогрева целиком нет: при N workers число соединений равно числу реально одновременно нужных.
- Создание соединения идёт вне глобального lock'а; в реестре slot резервируется со статусом `connecting`, при ошибке резервирование снимается.
- `persistent=false` — отдельное непостоянное соединение вне пула и вне лимита `max_connections_per_process`; закрывается физически при завершении lease (см. «Завершение lease»).
- **NTS (основной режим):** процесс однопоточен, освободить slot может только он сам. Если все `max_connections_per_process` slot'ов заняты, сразу бросается `RsDataBasePoolExhaustedException` без ожидания.
- **ZTS:** запросы ждут в FIFO-очереди на condition variable; `close()`/`__destruct()` будит первого; по `pool_wait_timeout_ms` бросается `RsDataBasePoolTimeoutException`. Это отдельная сборка со своими тестами.

### Проверка живости и восстановление

- При выдаче lease, если `idle > idle_validate_ms`, slot проверяется: Oracle — `Connection::ping()` (dpiConn_ping, без SQL), PostgreSQL — `Client::is_valid()`/пустой simple query.
- Free slot не содержит транзакции, поэтому при провале проверки он **прозрачно пересоздаётся** (с повторным применением session setup) до выдачи.
- TCP keepalive (параметр `keepalive_s`) снижает риск обрыва соединения firewall/NAT во время простоя.
- Ошибка соединения во время работы lease: slot помечается `broken`, текущая операция бросает `RsDataBaseConnectionException`, после `close()` slot не возвращается в пул, а закрывается.
- Heartbeat в фоне **не реализуется**. Если измерения покажут, что проверки при выдаче недостаточно (например, DRCP с агрессивным inactivity timeout), добавить отдельным этапом.

### Завершение lease

Завершение lease происходит одним и тем же кодом из трёх мест: явный `close()`, `__destruct()` (на практике в конце запроса, так как реестр держит ссылку) и `RSHUTDOWN`. Результат зависит от флага `persistent`, с которым соединение было получено:

| | `persistent = true` | `persistent = false` |
| --- | --- | --- |
| Незавершённая транзакция | откатывается | откатывается |
| Сброс сессии (`session_reset`) | выполняется | не нужен |
| Физическое соединение | **остаётся открытым**, slot возвращается в пул со статусом `free` и ждёт следующего запроса этого процесса | **закрывается**: Oracle — закрытие соединения, PostgreSQL — Terminate и закрытие сокета |
| Запись в реестре запроса | удаляется | удаляется |
| Связанные Prepare/Cursor | инвалидируются | инвалидируются |

- Если откат или сброс сессии у persistent-соединения не удался (ошибка, превышение внутреннего таймаута, `broken`), slot **закрывается** вместо возврата в пул и при следующей потребности создаётся заново.
- Повторный вызов `close()` на уже закрытом объекте безвреден (ничего не делает); `__destruct()` после `close()` тоже.
- Явный `close()` persistent-соединения в середине запроса возвращает slot в пул, следующий `getInstance()` с тем же адресом возьмёт его заново (включая сброс сессии при возврате). Если нужно держать соединение до конца запроса, `close()` вызывать не нужно.
- В конце PHP-запроса persistent-соединение **не закрывается**: освобождается только lease, а физическое соединение остаётся открытым и переиспользуется следующим PHP-запросом того же процесса. Закрыть физически нужно лишь не persistent-соединение. Между PHP-запросами не должны переходить только открытая транзакция и состояние сессии (иначе они утекут к чужому запросу).
- Для долгоживущих CLI-процессов (демоны, consumer'ы) конец запроса не наступает, поэтому `close()` нужно вызывать после каждой единицы работы (раздел «Модель хранения соединений»).

### Сброс сессии при возврате slot'а

При завершении **persistent**-lease через `close()`/`__destruct()`/`RSHUTDOWN` (если `session_reset` не `0`):

1. rollback незавершённой транзакции;
2. PostgreSQL — `DISCARD ALL` отдельным запросом (нельзя внутри блока транзакции); Oracle — `DBMS_SESSION.RESET_PACKAGE`;
3. любая ошибка или превышение внутреннего таймаута шагов 1–2 (жёстко 5 с, чтобы зависший сокет не блокировал `RSHUTDOWN`) → slot закрывается, а не возвращается как `free`.

Ограничения: Oracle `ALTER SESSION` (NLS, `CURRENT_SCHEMA` и т.п.) этим сбросом не откатывается. Код, который их меняет, обязан вернуть значения сам, расширение их не отслеживает. Сброс выполняется при каждом `close()`, в том числе в середине запроса, и стоит одного-двух round-trip.

Кеш разбора SQL (раздел 5) сброс сессии не затрагивает. Кеш подготовленных statements в драйвере живёт на соединении: Oracle — statement cache ODPI-C; PostgreSQL — server-side prepared statements, которые `DISCARD ALL` удаляет. Поэтому для PostgreSQL вместо `DISCARD ALL` допустим «мягкий» сброс без `DEALLOCATE ALL` (`RESET ALL`, `CLOSE ALL`, `UNLISTEN *`, `pg_advisory_unlock_all()`, `DISCARD TEMP`, `DISCARD SEQUENCES`); выбор уточнить измерениями (`session_reset=2`).

### Транзакции

```php
public function beginTransaction(): self;
public function commit(): self;
public function rollBack(): self;
public function inTransaction(): bool;
public function getTransactionLevel(): int;
public function close(): void;
public function __destruct();
public function getErrorCode(): ?string;
public function getErrorMessage(): ?string;
```

Состояние: `transaction_level` (явные уровни), `physical_transaction_active`, `tx_failed`.

- Автокоммит всегда выключен. Oracle — autocommit выключен (в crate `oracle` это значение по умолчанию, расширение его не включает); PostgreSQL перед первым statement выполняет `BEGIN`, если физическая транзакция не активна.
- `inTransaction()` = `transaction_level > 0`. Неявная физическая транзакция (после `SELECT` без `beginTransaction()`) на `inTransaction()` не влияет.
- Транзакция и `transaction_level` общие для всех держателей объекта (см. «Семантика `getInstance()`»): вложенный `beginTransaction()` из другого места кода создаёт savepoint, а не отдельную транзакцию.
- `beginTransaction()`: уровень 1 — SQL не отправляется (PostgreSQL гарантирует `BEGIN`); уровень ≥ 2 — `SAVEPOINT RSDB_<lease>_<level>`.
- `commit()`:
  - уровень ≤ 1 — `COMMIT` физической транзакции, `physical_transaction_active=false`;
  - уровень ≥ 2 — PostgreSQL `RELEASE SAVEPOINT`, Oracle только уменьшает уровень.
  - Если `tx_failed=true` (PostgreSQL), `COMMIT` **не отправляется** (он молча сделал бы rollback): бросается `RsDataBaseException`, транзакция остаётся до `rollBack()`.
- `rollBack()`: уровень ≤ 1 — полный `ROLLBACK`; уровень ≥ 2 — `ROLLBACK TO SAVEPOINT` (+ `RELEASE` в PostgreSQL), `tx_failed` сбрасывается.
- `RsDataBasePrepare::execute()` сам никогда не отправляет `COMMIT`. **Исключение Oracle:** DDL и PL/SQL с собственным `COMMIT`/автономными транзакциями фиксируют данные неявно. Это документируется; `physical_transaction_active` для Oracle отслеживается приблизительно.
- Следующий PostgreSQL statement после commit/rollback начинает новый `BEGIN`.
- `close()` всегда откатывает изменения, освобождает lease и удаляет запись из реестра экземпляров запроса; после него дочерние Prepare/Cursor недействительны (проверка по generation-id lease). Так как объект общий для всех, кто получил его через `getInstance()`, закрытие одним участником закрывает соединение для всех.

### Статементный rollback в PostgreSQL (`statement_rollback=on`)

Чтобы поведение совпадало с Oracle, каждый `execute()` внутри физической транзакции оборачивается в `SAVEPOINT RSDB_STMT` … `RELEASE SAVEPOINT RSDB_STMT`. При ошибке выполняется `ROLLBACK TO SAVEPOINT RSDB_STMT` + `RELEASE`, транзакция остаётся рабочей, `tx_failed` не выставляется. Цена — два дополнительных round-trip на statement (`SAVEPOINT` и `RELEASE`; при ошибке вместо `RELEASE` — `ROLLBACK TO` и `RELEASE`). Первый `SAVEPOINT` отправляется вместе с `BEGIN` одним simple-запросом; для горячих участков отключается параметром `statement_rollback=off` (тогда после ошибки транзакция в failed state, SQLSTATE 25P02, до `rollBack()`).

Не переносить дефекты текущего `PostgreQueryProvider`: не использовать `oci_commit()`/`oci_rollback()` для PostgreSQL и реально выполнять `BEGIN`.

## 5. `RsDataBasePrepare`

Создаётся только методом `RsDataBaseConnect::prepare()`.

### Константы

Значения — строки (в соответствии с `?string $type`):

```php
RsDataBasePrepare::CURSOR;     // 'cursor'
RsDataBasePrepare::COLLECTION; // 'collection'
RsDataBasePrepare::ARRAY;      // 'array'
RsDataBasePrepare::CLOB;       // 'clob'
RsDataBasePrepare::BLOB;       // 'blob'
RsDataBasePrepare::INTEGER;    // 'integer'
RsDataBasePrepare::FLOAT;      // 'float'   (новый: без него float терялся в строке)
RsDataBasePrepare::STRING;     // 'string'
RsDataBasePrepare::BOOLEAN;    // 'boolean'
RsDataBasePrepare::NULL;       // 'null'
```

Автоопределение при `type = null`: `int→INTEGER`, `float→FLOAT`, `bool→BOOLEAN`, `string→STRING`, `null→NULL`, `array→ARRAY`.

### Bind API

```php
public function createBind(): RsDataBaseBind;
public function bind(string $variableName, mixed &$value, int $size = -1, ?string $type = null, ?string $tdo = null): self;
public function bindValue(string $variableName, mixed $value, int $size = -1, ?string $type = null, ?string $tdo = null): self;
public function removeBind(string|RsDataBaseBind $bind): bool;
public function getBinds(): array;
```

- `bind()` — IN/OUT по ссылке; `bindValue()` — IN с собственной копией (owned).
- Имена `id` и `:id` равнозначны, нормализуются к `:id`; дубликаты проверяются после нормализации.
- `removeBind()` принимает имя (с `:` или без) либо объект; удалённый Bind отсоединён.
- `getBinds()` возвращает `RsDataBaseBind[]` в порядке создания.
- Bind, не использованный в SQL, и placeholder без Bind — ошибка `RsDataBaseException` при `execute()`.

### Разбор SQL (токенизатор)

Placeholder'ы ищет настоящий лексер, а не regexp. Он пропускает:

- строковые литералы (`'...'`, Oracle `q'[...]'`, PostgreSQL `E'...'`);
- quoted identifiers `"..."`;
- комментарии `--` и `/* */`;
- dollar-quoting `$tag$...$tag$` (PostgreSQL);
- `::` (cast PostgreSQL) и `:=` (присваивание PL/SQL, PL/pgSQL);
- `?`-операторы JSON не считаются placeholder'ами.

Для PostgreSQL named placeholders преобразуются в `$1..$N` без подстановки значений в SQL; повторное использование одного имени — один `$n`.

### Анонимные блоки и Oracle-специфичные конструкции

Лексер определяет вид запроса: `select`, `dml`, `block`, `returning_into`, `rejected`.

- **`isPLSQL()`** истинно, если первое значимое слово — `DECLARE`, либо `BEGIN`, после которого идёт тело блока. `BEGIN;`, `BEGIN TRANSACTION` и `START TRANSACTION` блоком не являются: это управление транзакцией, такой SQL отклоняется (`rejected`) с подсказкой использовать `beginTransaction()`. Так же отклоняются отдельные `COMMIT`, `ROLLBACK`, `SAVEPOINT`.
- **`COMMIT`/`ROLLBACK` внутри блока**: `DO` внутри открытой транзакции этого не допускает. Лексер находит такие операторы и возвращает понятную ошибку до обращения к БД.
- **`RETURN;` внутри блока** выходит из всей обёртки `DO`, после чего OUT-значения не записываются, и это происходит без ошибки. Пользовательский блок получает метку `<<rsdb_main>>`, а лексер заменяет голый `RETURN;` на `EXIT rsdb_main;`. `RETURN <выражение>` в `DO` недопустим и остаётся ошибкой PostgreSQL.
- **`INSERT/UPDATE/DELETE … RETURNING … INTO :a, :b`** (частый Oracle-паттерн): для PostgreSQL выполняется без `DO`. `INTO …` вырезается, запрос выполняется как обычный, значения первой строки записываются в соответствующие OUT-bind'ы. Ноль строк — OUT получают `null`, больше одной — ошибка. Для Oracle конструкция выполняется нативно.
- **Завершающий `/`** (SQL*Plus) на отдельной строке и отсутствие `;` после последнего `END` нормализуются.
- **Позиции ошибок.** PostgreSQL считает строку и позицию по всему `DO` вместе с обёрткой. При формировании `getErrorMessage()` расширение вычитает длину заголовка обёртки, чтобы позиция указывала в исходный SQL.

### Кеширование разбора SQL

Разбор SQL (токенизация, поиск placeholder'ов, определение вида запроса) — чистая функция от `(драйвер, текст SQL)`. Поэтому кеш разбора **не требует инвалидации по времени или по изменениям схемы**: результат для одного и того же текста всегда одинаков. Нужны только ограничение размера и защита от коллизий.

**Что кешируется** (`ParsedSql`, неизменяемая структура в `Arc`):
- последовательность сегментов SQL и список placeholder'ов с позициями;
- вид запроса: `select`, `dml`, `block`, `returning_into`; признак `isPLSQL()`;
- SQL после конвертации под драйвер (`$1..$N` для PostgreSQL);
- результат отклонения (SQL с `COMMIT`/`BEGIN;` внутри и т.п.), чтобы не разбирать его повторно.

**Что не кешируется:** значения bind'ов, типы bind'ов, случайный `execution_id` и dollar-тег. Обёртка анонимного блока (`DECLARE` с типами переменных, preamble, postamble) собирается из закешированных сегментов при каждом execute — это простое склеивание строк, дёшево, и тип bind'а не влияет на ключ.

**Ключ и валидация:**
- ключ — 128-битный хэш от `(driver_kind, sql)`; в записи хранится сам SQL, при попадании он сравнивается побайтно, чтобы коллизия хэша не вернула чужой разбор;
- в записи хранится `PARSER_VERSION`, константа компиляции; записи с другой версией не используются (защита, если логика парсера изменилась, хотя при перезапуске процесса кеш и так пуст).

**Область и вытеснение:**
- кеш процессный (статическая структура `.so`, как пул соединений) и живёт между запросами одного worker'а;
- вытеснение LRU по двум границам: `parse_cache_entries` и `parse_cache_max_bytes` (учитывается размер SQL и сегментов); при превышении удаляются самые давние записи;
- TTL не нужен: записи не устаревают, а неиспользуемые вытесняются LRU. Таймер или фоновая очистка не используются (принцип 1);
- SQL длиннее `parse_cache_max_sql_bytes` разбирается без кеширования, чтобы редкие огромные запросы не вытесняли горячие;
- динамический SQL со вшитыми литералами даёт много уникальных записей. Лимиты это сдерживают; счётчик промахов в статистике помогает такой код найти;
- Доступ через `Mutex` (требование Rust для `static`): в NTS он никогда не конкурирует, в ZTS критическая секция короткая (сам разбор выполняется до захвата lock'а).

**Диагностика:**

```php
public static function getParseCacheStats(): array; // entries, bytes, hits, misses, evictions
public static function clearParseCache(): void;
```

### Кеш statements на стороне драйвера и устаревшие планы

Это отдельный уровень, он живёт на соединении, и именно здесь возможна реальная устарелость:
- Oracle: statement cache ODPI-C (`stmt_cache_size` задаётся при подключении; для одноразовых запросов — `exclude_from_cache`).
- PostgreSQL: server-side prepared statements по хэшу SQL, кеш на slot'е.

Статус «устарел» возникает после DDL или смены типа результата. Обработка: при ошибке, возникшей на этапе prepare/describe и означающей устаревший statement (PostgreSQL `0A000` **только** вместе с сообщением «cached plan must not change result type», так как под `0A000` приходят и другие ошибки; `26000`; Oracle `ORA-04061`, `ORA-04062`, `ORA-04065`, `ORA-04068`, `ORA-01007`), запись удаляется из кеша соединения и statement подготавливается заново, **один раз**. Повтор безопасен только потому, что такие ошибки возникают до выполнения; для ошибок в процессе выполнения повтор не делается. Для PostgreSQL повтор происходит внутри statement-savepoint (раздел 4). Оговорка для Oracle: `ORA-04061/04068` означают, что состояние PL/SQL-пакетов сессии было сброшено; повтор вызова корректен, но состояние пакетов остаётся пустым, это нужно документировать для кода, зависящего от состояния пакетов.

Размер кеша statements на соединении ограничен (`stmt_cache_size` Oracle; для PostgreSQL — LRU на тот же лимит, `DEALLOCATE` вытесняемых). Кеш statements очищается вместе с соединением (broken, reconnect, закрытие slot'а).

### SQL, выполнение и выборка

```php
public function getSql(): ?string;
public function isPLSQL(): bool;            // анонимный блок: BEGIN|DECLARE в начале

public function execute(bool $reconnect = false): bool;
public function fetchRow(): ?array;
public function fetchRows(int $count): array;   // пакетная выборка
public function fetchAllRows(): array;
public function getAffectedRows(): int;
public function getColumnCount(): int;
public function getConnection(): RsDataBaseConnect;
public function free(): void;
public function __destruct();

public function getErrorCode(): ?string;
public function getErrorMessage(): ?string;
public function enableLog(): void;
public function disableLog(): void;
public function getLog(): array;
```

- `execute()` возвращает `true` при успехе и `false` при ошибке драйвера. Ошибки использования API и закрытого/сломанного соединения бросают исключение.
- Коды ошибок: Oracle — `ORA-xxxxx` (или код ODPI-C), PostgreSQL — пятисимвольный SQLSTATE.
- `execute(true)` разрешает reconnect **только** если нет активной физической транзакции и lease не `broken`; иначе — исключение (скрытый reconnect потерял бы транзакцию).
- Повторный `execute()` того же объекта закрывает предыдущий результат и выполняет statement заново с текущими значениями bind'ов. `fetch*()` до успешного `execute()` бросает исключение.
- `fetchAllRows()` читает результат целиком в память; для больших выборок использовать `fetchRows($count)`.
- `free()` освобождает statement/result/binds, но не соединение.
- SQL задаётся только в `RsDataBaseConnect::prepare()` и неизменяем; методов `parse()`, `setCacheKey()` и `getConvertSql()` нет. Разбор берётся из кеша автоматически (ключ — хэш SQL).

### Результат

| Источник | PHP-значение |
| --- | --- |
| Oracle `NUMBER` scale 0, влезает в i64 | `int` |
| Oracle `NUMBER`, иначе | `string` (без потери точности) |
| Oracle `BINARY_FLOAT/DOUBLE`, PG `float4/8` | `float` |
| PG `int2/4/8` | `int` |
| PG `numeric` | `string` |
| PG `boolean` | `bool` |
| `VARCHAR/TEXT/CHAR` | `string` |
| Oracle `DATE`, PG `date`/`timestamp` | `string` `Y-m-d H:i:s[.u]` (без timezone) |
| PG `timestamptz` | `string` ISO-8601 с offset |
| `CLOB` | `string` целиком (лимит `lob_max_bytes` — открытый вопрос) |
| `BLOB`, PG `bytea` | `string` (бинарная) |
| PG array | PHP `array` |
| Прочие типы PG (`uuid`, `json`, `jsonb`, `interval`, `inet` …) | `string` (текстовое представление) |
| SQL NULL | `null` |

- Ключи строки — имена колонок; регистр по `column_case` (по умолчанию `upper`, совместимо с OCI; **подтвердить по legacy `Query`**).
- Дублирующиеся имена колонок: побеждает последняя (как `oci_fetch_assoc`).
- `fetchRow()` возвращает `null` после последней строки.
- Типы PG декодируются по OID собственной реализацией `FromSql` (crate `postgres` не умеет отдавать произвольный тип как текст). Колонка неподдерживаемого типа вызывает исключение с именем колонки и OID, а не молчаливо возвращает мусор.

### Cursor

`CURSOR` — Oracle OUT REF CURSOR. После успешного `execute()` значение по ссылке становится объектом `RsDataBaseCursor` с методами `fetchRow()`, `fetchRows()`, `fetchAllRows()`, `getColumnCount()`, `free()`. Курсор привязан к lease и становится недействительным после `close()`/`commit()` соответствующей транзакции в PostgreSQL. Для PostgreSQL `CURSOR` поддерживается через `refcursor` (OUT возвращает имя portal, затем `FETCH` в той же транзакции) — этап 2; для обычных `SELECT` строки читаются через `fetchRow()` самого Prepare.

## 6. `RsDataBaseBind`

Создаётся только через `RsDataBasePrepare::createBind()`.

```php
public function setName(string $name): self;
public function setValue(mixed &$value): self;
public function setSize(int $size = -1): self;
public function setType(?string $type): self;
public function setTdo(?string $tdo): self;
```

- `setName('id')` и `setName(':id')` равнозначны.
- `setValue()` хранит Zend-ссылку и обновляет её для OUT/IN OUT только после полного успеха `execute()`.
- Ссылка живёт не дольше запроса: в `RSHUTDOWN` все Bind инвалидируются, Rust-код не выполняет Drop Zval вне PHP-потока.
- `setType(null)` включает автоопределение; `setTdo()` задаёт Oracle type descriptor для `COLLECTION`.
- После `removeBind()` объект отсоединён и не участвует в запросе.

## 7. PostgreSQL IN/OUT анонимных блоков

`DO` не поддерживает bind и OUT. Совместимый механизм: значения передаются через транзакционные настройки `set_config(..., true)`, а блок оборачивается в генерируемую обёртку. Пользовательский блок встраивается как вложенный; лексер заменяет в нём только placeholder'ы и `RETURN` (раздел 5), остальное не переписывается и не переводится: блок должен быть валиден для PL/pgSQL (расширение не транслирует PL/SQL в PL/pgSQL).

1. `bindValue()` — IN; `bind()`/`createBind()->setValue()` — IN/OUT. Значение `null` передаётся как SQL NULL, поэтому «чистый OUT» отдельной обработки не требует.
2. Для каждого execute генерируется `execution_id` (16 hex) и ключи `rsdb.b<execution_id>.p<index>`. Буквенные префиксы нужны, чтобы компоненты имени были валидными идентификаторами (проверить на целевых версиях PostgreSQL).
3. Значение в ключе кодируется с префиксом: `N` — SQL NULL, `V<text>` — значение. Это различает NULL и пустую строку (`set_config(key, NULL, true)` дал бы `''`).
4. IN передаются одним запросом: `SELECT set_config($1,$2,true), set_config($3,$4,true), ...`.
5. Генерируется блок со случайным dollar-тегом (`$rsdb_<hex>$`, проверяется отсутствие тега в SQL):

   ```sql
   DO $rsdb_ab12$
   DECLARE rsdb_v0 text; rsdb_v1 bigint;          -- по одной переменной на bind
   BEGIN
     rsdb_v0 := CASE WHEN left(current_setting('rsdb.b…p0'),1)='N' THEN NULL
                     ELSE substr(current_setting('rsdb.b…p0'),2)::text END;  -- для IN, IN/OUT; ::<тип переменной>
     <<rsdb_main>>
     BEGIN <пользовательский блок, :name → rsdb_v<index>> END rsdb_main;
     PERFORM set_config('rsdb.b…p1',
       CASE WHEN rsdb_v1 IS NULL THEN 'N' ELSE 'V'||rsdb_v1::text END, true);  -- для OUT, IN/OUT
   END $rsdb_ab12$
   ```

   Индекс — порядковый номер первого появления placeholder'а в SQL (стабилен, поэтому подходит для кеша разбора). Переменные именуются по индексу, а не по имени bind, чтобы не конфликтовать с колонками и переменными пользовательского кода. OUT записываются только после успешного завершения блока.
6. После блока все OUT читаются одним `SELECT current_setting($1,true), ...`.
7. Значения преобразуются обратно; PHP-ссылки обновляются только после успеха всех шагов.

Порядок внутри savepoint: `set_config` → `DO` → чтение OUT → `RELEASE`/`ROLLBACK TO`. Чтение OUT должно идти до `RELEASE`/`ROLLBACK TO`, иначе значения откатятся вместе с subtransaction. Один вызов с OUT — до пяти round-trip, что нормально для редких вызовов процедур, но не для горячих циклов (это нужно сказать в документации).

Весь execute выполняется внутри statement-savepoint (раздел 4). При ошибке: OUT-ссылки не обновляются, `execute()` возвращает `false`, SQLSTATE и сообщение сохраняются; при `statement_rollback=off` соединение остаётся в failed state до `rollBack()`.

Преобразования:

- `INTEGER ↔ bigint`, `FLOAT ↔ double precision`, `BOOLEAN ↔ boolean`, `STRING/CLOB ↔ text`;
- `ARRAY ↔ bigint[]` (если все элементы целые) или `text[]`;
- `COLLECTION`: `TDO → PG-тип` из явной таблицы соответствий (`D_CL_ID → bigint[]`, `D_CL_STR → text[]`), неизвестный TDO — ошибка;
- `BLOB ↔ bytea` (hex-представление `\x...`);
- литералы массивов строятся с корректным экранированием.

## 8. Примеры PHP

### Подключение, запрос и транзакция

```php
$db = RsDataBaseConnect::getInstance('pg://DEV:def@192.168.241.137:5432/DEV', true);

$query = $db->prepare('SELECT id, name FROM organisation WHERE active = :active');
$query->bindValue('active', true, -1, RsDataBasePrepare::BOOLEAN);

if (!$query->execute()) {
    $db->rollBack();
    throw new RsDataBaseException($query->getErrorMessage());
}

$rows = $query->fetchAllRows();
$db->commit();
$db->close();
```

### Одно соединение на адрес в пределах запроса

```php
$a = RsDataBaseConnect::getInstance('pg://DEV:def@192.168.241.137:5432/DEV', true);
$a->beginTransaction();

// в другом месте кода того же запроса
$b = RsDataBaseConnect::getInstance('pg://DEV:def@192.168.241.137:5432/DEV', true);
var_dump($a === $b);               // true
var_dump($b->getTransactionLevel()); // 1: транзакция общая

$b->beginTransaction();            // уровень 2: SAVEPOINT
$b->rollBack();                    // откат к savepoint
$a->commit();                      // фиксация внешней транзакции
```

### Все типы bind

```php
$query = $db->prepare($plSql);

$id = 42;
$query->createBind()->setName('id')->setValue($id)->setType(RsDataBasePrepare::INTEGER);

$name = 'Тест';
$query->createBind()->setName('name')->setValue($name)->setSize(255)->setType(RsDataBasePrepare::STRING);

$price = 12.5;
$query->createBind()->setName('price')->setValue($price)->setType(RsDataBasePrepare::FLOAT);

$enabled = true;
$query->createBind()->setName('enabled')->setValue($enabled)->setType(RsDataBasePrepare::BOOLEAN);

$empty = null;
$query->createBind()->setName('empty')->setValue($empty)->setType(RsDataBasePrepare::NULL);

$codes = ['A', 'B'];
$query->createBind()->setName('codes')->setValue($codes)->setSize(100)->setType(RsDataBasePrepare::ARRAY);

$ids = [10, 20];
$query->createBind()->setName('ids')->setValue($ids)
    ->setType(RsDataBasePrepare::COLLECTION)->setTdo('D_CL_ID');

$clob = str_repeat('Текст ', 1000);
$query->createBind()->setName('description')->setValue($clob)->setType(RsDataBasePrepare::CLOB);

$blob = file_get_contents('/path/to/file.bin');
$query->createBind()->setName('payload')->setValue($blob)->setType(RsDataBasePrepare::BLOB);

$cursor = null;
$query->createBind()->setName('resultCursor')->setValue($cursor)->setType(RsDataBasePrepare::CURSOR);

if (!$query->execute()) {
    throw new RsDataBaseException($query->getErrorMessage());
}
$rows = $cursor->fetchAllRows(); // RsDataBaseCursor
```

### PostgreSQL анонимный IN/OUT-блок

```php
$query = $db->prepare(<<<'SQL'
BEGIN
    :resultMessage := upper(:sourceMessage);
    :resultLength := length(:resultMessage);
END;
SQL);

$query->bindValue('sourceMessage', 'документ', 4000, RsDataBasePrepare::STRING);

$resultMessage = null;
$resultLength = null;
$query
    ->bind('resultMessage', $resultMessage, 4000, RsDataBasePrepare::STRING)
    ->bind('resultLength', $resultLength, -1, RsDataBasePrepare::INTEGER);

if (!$query->execute()) {
    $db->rollBack();
    throw new RsDataBaseException(sprintf('[%s] %s', $query->getErrorCode(), $query->getErrorMessage()));
}

$db->commit();
```

## 9. Внутренняя Rust-архитектура

- `RequestRegistry`: request-local `HashMap<canonical DSN, ZendObject-ссылка на RsDataBaseConnect>`; создаётся в `RINIT`, очищается в `RSHUTDOWN` (в ZTS — на поток). Ссылки на PHP-объекты есть только здесь.
- `RsDataBaseConnect`: `slot_id`, `lease_generation`, `physical_transaction_active`, `transaction_level`, `tx_failed`, ошибки, дочерние statements.
- `RsDataBasePrepare`: lease, SQL, binds, statement, result, ошибки, лог.
- `RsDataBaseBind`: weak parent, normalized name, `Reference|Owned`, size, type, TDO, attached status. Ссылка — только на PHP-стороне, живёт в пределах запроса.
- `RsDataBaseCursor`: weak parent, lease generation, handle курсора.
- `ConnectionRegistry`: `Mutex`, пулы по DSN, PID владельца, snapshots; condvar и FIFO — только в ZTS-сборке.
- `ConnectionSlot`: физическое соединение, masked address, `Connecting|Busy|Free|Broken`, `last_used`, persistent flag, owner lease.
- `DatabaseConnection`: enum `Oracle|Postgres`, один синхронный handle (без worker-потоков).
- `Dsn`/`DatabaseKind`: строгий парсинг, percent-decoding, параметры, canonical pool key (пароль — в хэше).
- `SqlTokenizer`: лексер из раздела 5.
- `BindValue`: преобразование PHP ↔ driver; Zval → owned Rust value до обращения к драйверу, OUT применяется после успеха.

Глобальный lock не удерживается во время network I/O. Panic guard обязан выполнить rollback/release и снять резерв slot'а.

## 10. Интеграция в Docker

- `docker/php/install-rs-database-connect.sh`.
- Rust/Clang build dependencies в `docker/php/install-build-deps.sh`.
- Исходники и install script в `docker/php/Dockerfile`; сборка Rust — в отдельной build-стадии (multi-stage), чтобы toolchain не попадал в runtime.
- Вызов установки из `docker/php/build-php-nts.sh`.
- Отдельный `.so` под каждую версию PHP 8.2–8.5 (bindings `ext-php-rs` генерируются под одну версию за сборку).
- Генерируемый ini: все INI-параметры из раздела 4 и `zend.exception_ignore_args=On` (production).
- ZTS: отдельный ABI/сборка либо явный запрет NTS `.so` на ZTS.
- В runtime переносить только `.so`, ini и необходимые Oracle/OpenSSL libraries.
- Обновить `docker/php/README.md` и `bin/README.md`.

## 11. Проверки

- **Unit:** кеш разбора: hit/miss, LRU-вытеснение по числу записей и по байтам, пропуск SQL больше лимита, защита от коллизии (одинаковый хэш, разный SQL), `PARSER_VERSION`, кеш отклонённого SQL, `parse_cache_entries=0`, ZTS-конкуренция; повтор при устаревшем statement (один раз, не повторяется после ошибки выполнения); DSN и параметры, регистронезависимые режимы, masking, pool key, ленивый рост, NTS-самоблокировка → `RsDataBasePoolExhaustedException`, ZTS FIFO/timeout/wakeup, idle-проверка и прозрачный reconnect free slot, broken slot, токенизатор (литералы, комментарии, `::`, `:=`, dollar-quoting), bind-преобразования, кодирование `N`/`V`, генерация DO-обёртки и dollar-тега, маппинг типов результата.
- **PHPT:** переиспользование persistent-соединения между запросами одного процесса (тот же slot, без нового подключения), `persistent_timeout_s`, классы, константы и их значения, сигнатуры, INI-параметры и валидация, aliases, исключения, `id`/`:id`, reference/owned, `RSHUTDOWN` с незакрытыми объектами и fatal error (соединение не утекает, состояние сброшено).
- **Завершение lease:** persistent — slot остаётся открытым и переиспользуется следующим запросом того же процесса (тот же backend PID / session id), не persistent — соединение закрыто (backend исчез на стороне БД); откат незавершённой транзакции в обоих случаях; сбой сброса сессии у persistent закрывает slot; повторный `close()`/`__destruct()` безвредны.
- **getNewInstance:** независимая транзакция (commit в новом соединении при rollback основного и наоборот), объект не возвращается `getInstance()` и не влияет на него; при `max_connections_per_process=1` и занятом `getInstance()` — `RsDataBasePoolExhaustedException`; `persistent=false` работает вне пула и лимита; `__destruct()` при потере последней ссылки откатывает и освобождает lease; не закрытый объект освобождается в `RSHUTDOWN`; повторное закрытие после `RSHUTDOWN` безвредно.
- **getInstance:** одинаковый адрес даёт тот же объект (`===`), эквивалентная запись адреса (регистр режима, порядок query-параметров) даёт тот же объект, разные адреса — разные; общая транзакция и вложенный `beginTransaction()` через savepoint у двух держателей; `unset($db)` не освобождает lease и не откатывает транзакцию; `close()` закрывает объект для всех, следующий `getInstance()` создаёт новый lease; расхождение `$persistent` при повторном вызове (первый флаг, `E_WARNING`); broken lease без транзакции заменяется, с транзакцией — нет; реестр очищается в `RSHUTDOWN`, включая `exit` и fatal error.
- **Oracle:** dedicated/DRCP, ленивый пул, исчерпание, idle-проверка при выдаче, обрыв соединения, no-auto-commit, rollback on close, savepoints, scalar/LOB/collection/cursor, `commit` при DDL, сброс пакетного состояния между lease.
- **PostgreSQL:** ленивый пул, named binds, anonymous IN/OUT (NULL vs `''`, массивы, bytea), SQLSTATE, обязательный `BEGIN`, savepoints, `statement_rollback` on/off (25P02), `commit()` в failed state, `DISCARD ALL` между lease, TLS-режимы.
- **Анонимные блоки:** `RETURN;` внутри блока (OUT записываются), `BEGIN;`/`COMMIT`/`ROLLBACK` как отдельный SQL и внутри блока (понятная ошибка), `INSERT … RETURNING … INTO` для обоих драйверов (0, 1, >1 строк), завершающий `/`, позиция ошибки относительно исходного SQL, один и тот же SQL с разными типами bind'ов (кеш разбора не должен менять результат).
- **Сессия:** `statement_timeout` и `application_name` переживают `DISCARD ALL` (стартовые параметры), сброс завершается по внутреннему таймауту на зависшем сокете, `close()` в середине запроса и повторный `getInstance()`.
- **Безопасность:** пароль отсутствует в `getConnections()`, исключениях, логах; fork-тест (дочерний процесс не использует соединения родителя).
- **Матрица:** PHP 8.0, 8.1, 8.2, 8.3, 8.4, 8.5 NTS — все обязательные (8.0 проверяется только нашим CI, upstream `ext-php-rs` его не тестирует); ZTS отдельно.
- **Надёжность:** Valgrind/ASan, отсутствие deadlock, отсутствие утечек соединений.

## 12. Порядок реализации

1. Каркас crate, DSN, реестр, жизненный цикл (`MINIT`…`MSHUTDOWN`), INI, исключения.
2. PostgreSQL: пул, транзакции, prepare/execute/fetch, токенизатор и кеш разбора, маппинг результата.
3. Oracle: dedicated, затем `:pooled` (DRCP), LOB, collections, cursor.
4. PostgreSQL IN/OUT анонимные блоки, `statement_rollback`, refcursor.
5. Сброс сессии, idle-проверка, кеш statements и обработка устаревших планов, TLS/таймауты, fork-защита.
6. Docker, PHPT/интеграционные тесты, ZTS-сборка.

## 13. Что изменено относительно редакции 1

| Было | Стало | Причина |
| --- | --- | --- |
| Прогрев ровно `pool_size` соединений | Ленивый рост до `max_connections_per_process` | 50 workers × 5 = лишние сотни соединений |
| Ожидание в FIFO при исчерпании | NTS — мгновенное исключение, FIFO — только ZTS | Один процесс сам себе slot не освободит, таймаут неизбежен |
| Фоновые heartbeat-worker'ы Oracle | По умолчанию отключены; optional feature после проверки драйвера | Безопасность и корректный lifecycle важнее автоматического ping; отдельный worker не является универсальным решением |
| Сброс context только при reconnect | Сброс сессии при каждом возврате slot'а, `RSHUTDOWN` | Утечка состояния между запросами |
| Ключ OUT = текст в `set_config` | Префикс `N`/`V` | NULL и `''` неразличимы |
| `DO $$` | Случайный dollar-тег, переменные по индексу | Коллизии с `$$` и с колонками |
| Ошибка → failed transaction | Statement-savepoint (по умолчанию) | Паритет с Oracle |
| `commit()` в failed state | Исключение | Тихий rollback вместо commit |
| Результат не описан | Таблица типов, `column_case`, `RsDataBaseCursor` | Пробел в спецификации |
| Нет TLS/таймаутов | Параметры DSN и INI | Production-требования |
| Нет `FLOAT` | Добавлен | float терялся |
| Разбор SQL при каждом вызове | Процессный LRU-кеш разбора (раздел 5) | Разбор — чистая функция, инвалидация по времени не нужна; нужны лимиты и защита от коллизий |
| Кеш statements не описан | Кеш на соединении, один повтор при устаревшем statement | Реальная устарелость возникает после DDL |
| `pool_size` | `max_connections_per_process` | Параметр действует на один процесс, прежнее имя читалось как общий лимит |
| Нет таймаута простоя slot'а | `persistent_timeout_s` | Аналог `oci8.persistent_timeout`, освобождение соединений после пика |
| `getInstance()` без определённой семантики | Одинаковый адрес в запросе — тот же объект и то же соединение, реестр экземпляров запроса | Требование legacy-кода (подтверждено) |
| `keepalive_s` для Oracle | Перевод в минуты `expire_time` | `EXPIRE_TIME` в Oracle задаётся в минутах |
| `statement_timeout`/`application_name` через `SET` | Стартовые параметры соединения | `RESET ALL`/`DISCARD ALL` сбросил бы их |
| `OCI_NO_AUTO_COMMIT` | autocommit выключен (по умолчанию в crate `oracle`) | Не терминология crate `oracle` |
| `RETURN`, `BEGIN;`, `COMMIT` и `RETURNING INTO` в блоках | Правила лексера (раздел 5) | Тихая потеря OUT, путаница с управлением транзакциями |
| `0A000` как признак устаревшего плана | `0A000` + проверка сообщения | Под `0A000` приходят и другие ошибки |
| `parse()`, `setCacheKey()`, `getConvertSql()`, `prepare(?string)` | Методы удалены, `prepare(string $sql)`, кеш разбора автоматический | Решение владельца: не нужны |
| Отдельный метод независимого соединения отсутствовал | `getNewInstance()` | Отдельная транзакция к тому же адресу |
| PHP 8.0 best-effort | PHP 8.2–8.5, все обязательные | Решение владельца; проверка 8.0 лежит на нашем CI |
| Модель хранения не описана | Раздел «Модель хранения соединений», расчёт нагрузки на БД | Исключить ожидание общего пула между процессами |

## 14. Открытые вопросы

1. ~~Семантика `parse`, `setCacheKey`, `getConvertSql`~~ — снято: методы удалены.
2. Регистр ключей результата в legacy `Query` (принят `upper`).
3. Лимит размера LOB при чтении целиком (`lob_max_bytes`) и потоковое чтение.
4. ~~Минимум PHP 8.1 вместо 8.0~~ — снято: поддерживаются 8.2–8.5.
5. Heartbeat DRCP — optional feature; включать только после доказательства thread-safety и тестов на целевом Oracle/ODPI-C стеке.
6. Валидность имён `rsdb.b<id>.p<n>` на всех целевых версиях PostgreSQL.
7. Мягкий сброс PostgreSQL без `DEALLOCATE ALL` (`session_reset=2`) против `DISCARD ALL`: сравнить выигрыш от сохранения prepared statements и риски.
8. Нужен ли общий (межпроцессный) лимит соединений. По умолчанию нет: решается `pm.max_children`, DRCP или PgBouncer.
9. Расхождение `$persistent` при повторном `getInstance()` с тем же адресом: принятое решение — побеждает первый флаг с `E_WARNING`. Подтвердить или заменить на исключение.
10. ~~Нужен ли `getNewInstance()`~~ — решено: нужен, добавлен.
11. Подтвердить, что ext-php-rs позволяет держать сильную ссылку на PHP-объект в request-local реестре и корректно снимать её в `RSHUTDOWN` (в том числе после `zend_call_destructors`).
12. ~~Порядок `RSHUTDOWN` относительно `session` при хранении сессий в БД~~ — снято: сессии хранятся в Redis, БД используется только для запросов за данными. Если схема хранения сессий изменится на БД через это расширение, вернуться к вопросу (расширение деактивируется раньше `session`, если загружено позже; решение — `post_deactivate_func` или порядок загрузки).
13. Подтвердить, что синхронный `postgres` crate не создаёт фоновых потоков (принцип 1).


---

# Дополнение к редакции 3: эксплуатация, сборка и распространение

> Это дополнение фиксирует решения по persistent-соединениям, кэшу парсера, проверке загрузки расширения, системным требованиям и распространению бинарных сборок. Требования, зависящие от конкретной версии PHP, ОС, Oracle Instant Client или ABI, должны подтверждаться тестами CI и матрицей релизных сборок.

## 1. Поддерживаемые версии PHP

- Минимальная версия PHP: **8.2**.
- Целевой диапазон текущей матрицы: PHP 8.2–8.5, если соответствующая версия PHP и `ext-php-rs` поддерживаются используемым toolchain.
- Сборка должна выполняться и проверяться отдельно для каждой поддерживаемой версии PHP.
- Необходимо фиксировать версии Rust, `ext-php-rs`, Cargo-зависимостей и инструментария сборки. `Cargo.lock` включается в репозиторий.
- Поддержка NTS/ZTS определяется отдельными сборками и тестами; нельзя считать бинарники NTS и ZTS взаимозаменяемыми.

## 2. Persistent-соединения: жизненный цикл и SAPI

### 2.1 Общая модель

Persistent-пул является **локальным для процесса PHP**. Соединения не разделяются между разными процессами и не образуют общий пул на уровне всего сервера.

При завершении PHP-запроса соединение, помеченное как persistent, не должно автоматически закрываться: расширение должно выполнить безопасный сброс состояния сессии и вернуть соединение в пул. Если сброс завершается ошибкой, соединение закрывается, а не возвращается в пул в потенциально загрязнённом состоянии.

Для неперсистентного соединения при завершении запроса выполняется rollback незавершённой транзакции и закрытие соединения. Точный порядок очистки и освобождения PHP-объектов должен быть подтверждён интеграционными тестами для каждой поддерживаемой версии PHP.

### 2.2 Поведение по средам

| Среда | Область жизни пула | Практическое поведение |
|---|---|---|
| Apache с `mod_php` | Один пул на процесс Apache/PHP | Запросы, обслуженные одним и тем же процессом, могут повторно использовать соединения. Между процессами Apache пул не общий. |
| PHP-FPM | Один пул на worker-процесс | Последующие запросы того же worker могут использовать соединение повторно. У каждого worker собственный пул. Перезапуск worker уничтожает его пул. |
| Другой сервер с PHP через FastCGI | Обычно пул на PHP worker-процесс | Поведение зависит от того, как сервер запускает и переиспользует PHP-процессы. Проверить на каждом целевом SAPI. |
| CLI: отдельный запуск скрипта | Один процесс на запуск | Соединение живёт только в рамках этого запуска; между двумя отдельными командами `php script.php` повторного использования нет. |
| CLI: долгоживущий worker/демон | Один пул на процесс | Повторное использование возможно между задачами внутри одного процесса. Необходимо контролировать транзакции, состояние сессии, обработку ошибок и явное закрытие. |

`RSHUTDOWN` относится к завершению PHP-запроса, а не обязательно к завершению процесса. Для CLI один запуск скрипта обычно соответствует одному PHP-запросу; долгоживущие приложения могут выполнять множество задач в одном процессе, но жизненный цикл объектов и соединений нужно тестировать с учётом конкретной модели запуска.

### 2.3 Ограничения и проверки

- Не обещать общий пул между Apache/FPM worker-процессами.
- При `pm.max_children`, числе процессов Apache и количестве инстансов приложения суммарное число соединений может быть значительно выше лимита, заданного для одного процесса.
- Для общего ограничения соединений использовать настройки сервера приложений, лимиты СУБД, PgBouncer для PostgreSQL или DRCP для Oracle, если они подходят инфраструктуре.
- При fork-подобных сценариях не допускать использования унаследованных соединений родительского процесса в дочернем процессе. Добавить защиту/тесты, где это применимо.
- Проверить поведение при аварийном завершении worker, рестарте процесса, сетевом обрыве и ошибке сброса сессии.
- Для PostgreSQL отдельно проверить выбранную стратегию сброса сессии (в том числе вариант с `DISCARD ALL`) и её влияние на подготовленные выражения.
- Для Oracle отдельно проверить reset сессии и поведение при использовании DRCP.

## 3. Кэш парсера SQL

### 3.1 Где хранится

Кэш парсера — **в памяти процесса**, в статической структуре расширения, внутри загруженной библиотеки. Он не хранится в файле, не переживает завершение процесса и не разделяется между разными PHP worker-процессами.

Следствия:
- у каждого worker свой независимый кэш;
- рестарт worker очищает кэш;
- CLI-процесс использует кэш до завершения процесса;
- кэш хранит результат разбора SQL, а не значения параметров/bind-переменных;
- SQL, превышающий лимит размера для кэширования, разбирается без помещения результата в кэш.

### 3.2 Управление через INI

Предлагаемые параметры (названия необходимо закрепить тестами и документацией перед стабильным релизом):

```ini
rs_database_connect.parse_cache_entries=512
rs_database_connect.parse_cache_max_bytes=4194304
rs_database_connect.parse_cache_max_sql_bytes=65536
```

| Параметр | Назначение |
|---|---|
| `parse_cache_entries` | Максимальное число записей кэша |
| `parse_cache_max_bytes` | Приблизительный общий лимит памяти кэша |
| `parse_cache_max_sql_bytes` | Максимальный размер SQL, для которого результат разбора допускается кэшировать |

Кэш использует LRU-вытеснение при достижении лимитов. TTL не предусмотрен. Добавить API для диагностики и управления:
- `getParseCacheStats()` — статистика попаданий/промахов, числа записей и использования памяти, если эти показатели реализованы;
- `clearParseCache()` — очистка кэша текущего процесса.

Статические данные должны быть безопасны при конкурентном доступе в рамках поддерживаемой модели выполнения. Реализацию блокировок и поведение ZTS подтвердить отдельными тестами. Не включать bind-значения, секреты или содержимое пользовательских данных в ключи/логи кэша.

## 4. Проверка загрузки расширения

После установки проверять расширение отдельно в CLI и в веб-SAPI: они могут использовать разные бинарники PHP, `php.ini`, `extension_dir` и наборы модулей.

### CLI

```bash
php --version
php --ini
php -m
php --ri rs_database_connect
php -r 'var_dump(extension_loaded("rs_database_connect"));'
```

Ожидаемый результат: модуль отображается в `php -m`, команда `php --ri rs_database_connect` выводит сведения и INI-параметры, а `extension_loaded()` возвращает `true`.

### Web-SAPI

- Проверить `phpinfo()` в защищённой диагностической среде или временном закрытом endpoint.
- Убедиться, что присутствует секция `rs_database_connect` и видны зарегистрированные INI-параметры.
- Проверить именно тот SAPI и тот PHP-FPM/Apache worker, который обслуживает приложение.
- Удалить диагностический endpoint после проверки: публичный `phpinfo()` в production не оставлять.

Также проверять журнал ошибок PHP при запуске: несовместимый ABI, отсутствующая зависимая библиотека или ошибка загрузки расширения могут приводить к тому, что модуль не появится в `php -m`.

## 5. Платформы и матрица бинарных сборок

Целевые базовые платформы:
- Linux;
- Windows;
- macOS.

Поддержка платформ означает не один универсальный файл, а проверенную матрицу артефактов. Для каждого релиза фиксировать:
- ОС и архитектуру процессора;
- версию PHP и ABI;
- NTS или ZTS, где применимо;
- версию Rust/toolchain и зависимостей;
- наличие и версию системных runtime-библиотек;
- поддержку PostgreSQL и/или Oracle в конкретном артефакте.

Типичные имена файлов: `.so` на Linux; PHP-расширения Windows распространяются как `.dll`; на macOS PHP-расширение также обычно имеет расширение `.so`. Расширение файла само по себе не гарантирует совместимость.

**Один собранный артефакт нельзя считать универсальным** для Windows/Linux/macOS, разных архитектур, PHP ABI или NTS/ZTS. Даже в пределах Linux необходимо учитывать системный ABI и минимальную версию системных библиотек. В CI нужно собирать и тестировать каждую заявленную комбинацию; неподтверждённые комбинации не указывать как поддерживаемые.

## 6. Сборка и подключение

### 6.1 Зависимости на машине сборки

Минимальный набор зависит от выбранных драйверов и целевой платформы:
- Rust toolchain, совместимый с закреплёнными версиями зависимостей;
- Cargo;
- PHP development headers и инструменты для сборки PHP-расширений, соответствующие целевой версии PHP;
- C/C++ toolchain и системные средства сборки;
- OpenSSL development-пакеты для сборок, использующих `postgres-openssl`;
- клиентские библиотеки/SDK Oracle и настройки ODPI-C для Oracle-сборки;
- средства сборки и тестирования, необходимые выбранной ОС.

Конкретные названия пакетов ОС и поддерживаемые версии библиотек нужно оформить в инструкциях для Linux, Windows и macOS после проверки в CI.

### 6.2 Зависимости на production-сервере

Production-серверу не требуется Rust toolchain, если на него поставляется готовый бинарный артефакт. Однако должны присутствовать все runtime-зависимости, которые не включены/статически не связаны с артефактом.

- Для PostgreSQL проверить runtime-зависимости OpenSSL и другие динамические библиотеки, если они используются собранным артефактом.
- Для Oracle требуется совместимый Oracle Instant Client/runtime, если Oracle-поддержка включена в сборку.
- Убедиться, что динамический загрузчик ОС находит все необходимые библиотеки.
- Версии и способ установки зависимостей должны быть задокументированы отдельно для каждой платформы.

### 6.3 Сборка и установка

1. Выбрать целевую комбинацию ОС, архитектуры, PHP-версии и NTS/ZTS.
2. Установить инструменты сборки и development headers соответствующего PHP.
3. Установить и настроить клиентские зависимости для включённых драйверов.
4. Собрать расширение в release-режиме с зафиксированным `Cargo.lock`.
5. Проверить результат на целевой среде: загрузку модуля, INI-параметры, подключение к тестовым базам и интеграционные тесты.
6. Установить бинарный файл в `extension_dir` именно используемого PHP.
7. Добавить в соответствующий `php.ini` или файл конфигурации SAPI:

```ini
extension=rs_database_connect
```

8. Добавить нужные `rs_database_connect.*` INI-параметры.
9. Перезапустить соответствующий сервис PHP/веб-сервера.
10. Проверить модуль командами CLI и через закрытую диагностическую страницу web-SAPI.
11. Проверить журнал ошибок и выполнить smoke-тест запросов к каждой включённой СУБД.

Фактическую команду Cargo и дополнительные переменные окружения следует закрепить в README после стабилизации layout crate и build-скриптов. Не копировать бинарник в production без проверки совместимости с конкретной средой.

## 7. Минимальные системные требования и зависимости

| Компонент | Минимальное требование / правило |
|---|---|
| PHP | 8.2 или новее в пределах протестированной матрицы |
| Rust | Версия, закреплённая проектом и совместимая с `ext-php-rs` |
| `ext-php-rs` | Закреплённая версия 0.15.x; точную версию фиксировать в зависимостях |
| PostgreSQL | Доступный сервер совместимой версии; сетевые настройки и TLS — по конфигурации приложения |
| PostgreSQL TLS | OpenSSL и его runtime-зависимости, если используются динамические библиотеки |
| Oracle | Совместимая версия Oracle Instant Client и настроенный ODPI-C для Oracle-сборки |
| ОС | Linux, Windows, macOS — только в пределах опубликованной матрицы тестирования |
| Архитектура | Каждая архитектура должна собираться и тестироваться отдельно |
| NTS/ZTS | Отдельно подтверждать совместимость и тесты; не смешивать артефакты |
| Production runtime | Только runtime-зависимости бинарника; компилятор Rust не требуется при установке готовой сборки |

Это минимальная проектная матрица, а не заявление о поддержке всех возможных сочетаний версий. Точные нижние границы версий PostgreSQL, Oracle Instant Client, OpenSSL и системных библиотек следует установить по результатам CI и зафиксировать в release-матрице.

## 8. План проверок перед выпуском

- [ ] CI-сборка и тесты для Linux, Windows и macOS.
- [ ] Проверка каждой заявленной версии PHP начиная с 8.2.
- [ ] Проверка NTS/ZTS для каждой комбинации, заявленной как поддерживаемая.
- [ ] Проверка `php -m`, `php --ri rs_database_connect`, `extension_loaded()` и INI-параметров.
- [ ] Проверка загрузки в CLI и в реальном web-SAPI.
- [ ] Проверка повторного использования persistent-соединения внутри одного процесса.
- [ ] Проверка изоляции пулов между процессами и корректного поведения при рестарте worker.
- [ ] Проверка CLI короткого запуска и долгоживущего процесса.
- [ ] Проверка rollback/reset и закрытия соединения при ошибке сброса.
- [ ] Проверка кэша парсера: лимиты, LRU, очистка, статистика, большие SQL и конкурентный доступ.
- [ ] Проверка отсутствующих runtime-зависимостей и документирование диагностических сообщений.
- [ ] Проверка PostgreSQL и Oracle отдельно для соответствующих сборок.
- [ ] Публикация release-матрицы и инструкций установки для каждой поддерживаемой платформы.

## 9. Вопросы, которые остаются открытыми до подтверждения тестами

| Вопрос | Что необходимо сделать |
|---|---|
| Точная матрица PHP 8.2+ | Подтвердить версии PHP и `ext-php-rs`, доступные в CI |
| Windows/macOS | Проверить сборку, загрузку и тесты на реальных runner'ах; не считать поддержку подтверждённой только по успешной компиляции |
| ZTS | Подтвердить поддержку и безопасность конкурентного доступа |
| PostgreSQL reset | Сравнить варианты очистки сессии и влияние на prepared statements |
| Oracle reset/DRCP | Подтвердить reset сессии и повторное использование соединений |
| LOB и потоковое чтение | Зафиксировать лимиты памяти и API потокового чтения |
| Кэш парсера | Подтвердить измерение памяти, метрики и поведение LRU |
| Runtime-библиотеки | Составить список динамических зависимостей для каждого релизного артефакта |
| Лимит соединений | Документировать, что лимит пула применяется на процесс, если отдельно не реализован внешний механизм координации |

## 10. Критерии готовности релиза

Релиз готов к публикации, когда:
1. Все объявленные сочетания ОС/архитектуры/PHP/NTS-ZTS собраны и протестированы.
2. Установка и загрузка описаны пошагово для CLI и web-SAPI.
3. Runtime-зависимости и требования к клиентам СУБД задокументированы.
4. Поведение persistent-соединений и SQL-кэша покрыто тестами.
5. Бинарные артефакты однозначно маркированы целевой платформой и ABI.
6. Непроверенные комбинации явно обозначены как неподдерживаемые или экспериментальные.


---

# Дополнение: CI/CD — GitHub Actions и Jenkins

## 11. Цели CI/CD

Настроить единый набор проверок для pull request, основной ветки и релизов. GitHub Actions и Jenkins должны запускать одинаковые логические этапы и использовать одинаковые правила качества, чтобы результаты не расходились между CI-системами.

Основные цели:
- проверять форматирование, компиляцию, тесты и качество Rust-кода;
- проверять совместимость с заявленными версиями PHP начиная с PHP 8.2;
- собирать артефакты для поддерживаемых ОС/архитектур и вариантов PHP ABI;
- тестировать загрузку расширения и его базовую работу с PostgreSQL и Oracle там, где доступны соответствующие окружения;
- сохранять логи, тестовые отчёты и бинарные артефакты;
- создавать релизные артефакты только из проверенного commit/tag.

## 12. Этапы CI pipeline

Рекомендуемый порядок этапов:

1. **Checkout и идентификация сборки**
   - получить исходный код;
   - записать commit SHA, ветку/tag и версию Rust/PHP в метаданные сборки;
   - использовать `Cargo.lock` и воспроизводимую конфигурацию зависимостей.

2. **Проверка исходников**
   - `cargo fmt --all -- --check`;
   - `cargo clippy --all-targets --all-features -- -D warnings` — если выбранные feature combinations и зависимости позволяют выполнить эту команду;
   - проверка отсутствия случайно закоммиченных секретов и локальных конфигураций;
   - проверка документации и примеров конфигурации.

3. **Rust unit tests**
   - `cargo test --locked`;
   - отдельно запускать тесты с feature flags, которые используются в релизных сборках;
   - не считать unit tests заменой PHP integration tests.

4. **Сборка PHP-расширения**
   - сборка release-артефакта для каждой заявленной комбинации ОС, архитектуры, версии PHP и NTS/ZTS;
   - использовать соответствующие PHP development headers/toolchain;
   - фиксировать зависимости и передавать метаданные артефакта;
   - не собирать один бинарник и не распространять его как универсальный для всех платформ.

5. **Проверка загрузки**
   - установить артефакт во временное тестовое PHP-окружение;
   - проверить `php --version`, `php --ini`, `php -m`, `php --ri rs_database_connect`;
   - проверить `extension_loaded("rs_database_connect")`;
   - сохранить stderr и логи PHP при ошибке загрузки.

6. **PHP integration tests / PHPT**
   - запускать PHPT и интеграционные тесты на соответствующей версии PHP;
   - проверить API подключения, транзакции, bind-параметры, fetch, исключения и очистку ресурсов;
   - тестировать persistent/non-persistent соединения в нескольких последовательных PHP-запросах одного worker там, где это поддерживает тестовая среда;
   - проверять кэш SQL-парсера: лимиты, очистку, статистику и работу при больших SQL;
   - проверять CLI отдельно от web-SAPI.

7. **Интеграционные тесты СУБД**
   - PostgreSQL: поднимать отдельный тестовый сервис/контейнер и выполнять тесты соединения, транзакций, сброса сессии и TLS, если он включён;
   - Oracle: запускать тесты только в runner/окружении, где доступны совместимый Oracle Instant Client и тестовая БД;
   - если Oracle недоступен в обычном PR pipeline, явно отмечать тесты как пропущенные/отложенные, а не как успешно пройденные;
   - секреты подключения получать из защищённого хранилища CI, не записывать их в логи.

8. **Проверка runtime-зависимостей**
   - Linux: проверять динамические зависимости собранного файла средствами целевой системы (например, `ldd` в доверенном окружении);
   - Windows/macOS: проверять загрузку в соответствующем runner и наличие необходимых runtime-библиотек;
   - отдельно проверять Oracle runtime-зависимости для Oracle-сборки;
   - проверять артефакты на целевой минимальной версии ОС/ABI, если она заявлена.

9. **Публикация артефактов**
   - для pull request — хранить диагностические артефакты в рамках CI run, если это разрешено политикой проекта;
   - для tag/release — публиковать только артефакты, прошедшие обязательные проверки;
   - прикладывать manifest с ОС, архитектурой, PHP-версией, NTS/ZTS, commit SHA и runtime-зависимостями;
   - не перезаписывать уже опубликованные релизные артефакты.

## 13. Матрица CI

Матрица определяется только реально доступными runner'ами и подтверждёнными сборками. В качестве целевой структуры:

| Ось матрицы | Значения |
|---|---|
| ОС | Linux, Windows, macOS |
| PHP | Каждая заявленная версия начиная с 8.2 |
| Архитектура | Каждая архитектура, которую проект собирается официально поддерживать |
| PHP build mode | NTS/ZTS — только там, где поддержка заявлена и проверяется |
| Драйверы | PostgreSQL; Oracle — в окружениях с доступным Instant Client и тестовой БД |
| Тип проверки | lint/format, build, unit tests, extension load, PHP integration, DB integration |

Не обязательно запускать все тяжёлые интеграционные тесты на каждом pull request. Допустимая стратегия:
- **PR:** быстрые проверки, unit tests, сборка и загрузка расширения для основной матрицы; PostgreSQL integration tests на основных целевых комбинациях;
- **main/nightly:** расширенная матрица, тесты persistent-пула, стресс-тесты и дополнительные варианты PHP/SAPI;
- **release tag:** полный набор обязательных тестов для всех объявленных релизных артефактов;
- **Oracle:** обязательный release/nightly job в специальном окружении, если Oracle включён в опубликованный артефакт.

Любая комбинация, которая не тестируется автоматически или вручную, не должна без оговорки считаться полностью поддерживаемой.

## 14. GitHub Actions

Рекомендуемая структура workflow-файлов:

```text
.github/
  workflows/
    ci.yml
    integration.yml
    release.yml
```

### `ci.yml`

Запускать на pull request и push в основную ветку:
- checkout;
- установка закреплённого Rust toolchain;
- установка требуемого PHP toolchain для текущего runner;
- cache Cargo registry/build artifacts с ключом, учитывающим ОС, toolchain и `Cargo.lock`;
- `cargo fmt --all -- --check`;
- `cargo clippy ...`, если эта конфигурация поддерживается;
- `cargo test --locked`;
- build extension;
- load check через `php -m` и `php --ri`;
- загрузка логов/отчётов при неуспешном job.

### `integration.yml`

Запускать на pull request, по расписанию и вручную:
- PostgreSQL service/container;
- подготовка тестовой схемы и учётных данных через CI secrets;
- PHP integration/PHPT tests;
- отдельные jobs для CLI и web-SAPI, если доступны подходящие runner'ы;
- отдельный Oracle job на self-hosted runner или защищённом окружении с Instant Client и доступом к тестовой БД.

### `release.yml`

Запускать по тегу релиза:
- проверить, что обязательные CI jobs завершились успешно;
- собрать артефакты release-матрицы;
- выполнить финальную проверку загрузки и runtime-зависимостей;
- создать manifest/checksum-файлы;
- опубликовать артефакты релиза;
- приложить инструкции по установке и список поддерживаемых комбинаций.

### Требования безопасности GitHub Actions

- Для сторонних actions фиксировать версии; для строгой цепочки поставки предпочтительно закреплять actions по полному commit SHA.
- Не выдавать токены публикации обычным PR jobs.
- Не запускать недоверенный код из fork с доступом к секретам.
- Выдавать минимально необходимые `permissions`.
- Использовать защищённые environments для релиза и публикации.
- Не печатать DSN, пароли, токены и содержимое секретных переменных в логах.

## 15. Jenkins

Рекомендуется хранить `Jenkinsfile` в корне репозитория и описать runner labels для разных платформ.

Примерная структура pipeline:

```groovy
pipeline {
    agent none

    options {
        timestamps()
        disableConcurrentBuilds()
    }

    stages {
        stage('Validate') {
            agent { label 'rust-builder' }
            steps {
                checkout scm
                sh 'cargo fmt --all -- --check'
                sh 'cargo test --locked'
            }
        }

        stage('Build and load-check') {
            agent { label 'php-extension-builder' }
            steps {
                checkout scm
                // Установка toolchain/PHP и команды сборки
                // должны быть определены в репозитории для конкретной платформы.
                sh 'cargo build --release --locked'
                // После установки артефакта в тестовый PHP:
                // php -m
                // php --ri rs_database_connect
            }
        }

        stage('Integration tests') {
            agent { label 'php-db-integration' }
            steps {
                checkout scm
                // Запуск PHPT/PHP integration tests.
                // PostgreSQL/Oracle credentials передавать через Jenkins Credentials.
            }
        }

        stage('Archive artifacts') {
            when {
                anyOf {
                    branch 'main'
                    buildingTag()
                }
            }
            agent { label 'php-extension-builder' }
            steps {
                archiveArtifacts artifacts: 'dist/**,reports/**', allowEmptyArchive: true
            }
        }
    }

    post {
        always {
            // При необходимости публиковать JUnit/PHPT отчёты,
            // собрав их в формат, поддерживаемый установленными плагинами Jenkins.
            cleanWs()
        }
    }
}
```

Это каркас pipeline, а не готовый универсальный `Jenkinsfile`: команды сборки должны быть привязаны к фактическому build-скрипту проекта и к конкретному runner. Например, `sh` работает в Unix-подобном агенте; для Windows-агентов использовать соответствующий `bat`/PowerShell шаг или отдельную ветку pipeline.

### Требования к Jenkins-инфраструктуре

- Подготовить отдельные agents/labels для Linux, Windows и macOS, если все эти платформы входят в релизную матрицу.
- Установить на agents нужные версии Rust, PHP toolchain, компилятор и клиентские зависимости.
- Использовать Jenkins Credentials для DSN/паролей и ограничивать их область доступности.
- Не запускать несколько сборок в одном изменяемом рабочем каталоге; изолировать workspace и очищать его после выполнения.
- Публиковать JUnit/PHPT-отчёты и архивировать логи при сбоях.
- Отделить права на сборку PR от прав на публикацию релиза.
- Для Oracle использовать выделенный защищённый agent с установленным Instant Client и доступом к тестовой БД.
- Настроить тайм-ауты и явную очистку тестовых ресурсов, чтобы зависшие тесты не занимали agents бесконечно.

## 16. Единые критерии прохождения CI

Обязательные проверки:
- [ ] Форматирование Rust проходит.
- [ ] Unit tests проходят.
- [ ] Все объявленные обязательными target builds успешно собираются.
- [ ] Расширение загружается целевой версией PHP.
- [ ] Модуль виден в `php -m`, а `php --ri rs_database_connect` работает.
- [ ] PHP integration/PHPT tests проходят.
- [ ] PostgreSQL tests проходят для заявленных PostgreSQL-конфигураций.
- [ ] Oracle tests проходят в обязательном Oracle release/nightly job, если Oracle заявлен для релиза.
- [ ] Проверены runtime-зависимости и metadata каждого артефакта.
- [ ] Релиз не публикуется при провале обязательного job.
- [ ] Логи не содержат секретов.

## 17. Решения по внедрению CI/CD

1. Сначала реализовать минимальный GitHub Actions или Jenkins pipeline для Linux + одной поддерживаемой PHP-версии: format, unit tests, build, load-check.
2. Затем добавить матрицу PHP 8.2+ и остальные заявленные ОС/архитектуры.
3. После стабилизации сборки добавить PostgreSQL integration tests.
4. Поднять отдельное защищённое окружение для Oracle integration tests.
5. Добавить nightly расширенную матрицу и тесты persistent-пула/кэша.
6. Подключить release pipeline, manifest и checksum-файлы.
7. Поддерживать одну общую спецификацию проверок независимо от того, запускаются они в GitHub Actions или Jenkins.

Открытые детали для уточнения в репозитории: точная команда сборки расширения, формат запуска PHPT, список runner'ов, поддерживаемые версии PHP/архитектуры, доступность Oracle CI-среды и правила публикации релизов.

---

# Дополнение: долгие синхронные операции, Oracle keepalive и потеря соединения

## Риск и требование

При синхронном выполнении длительной операции PHP-поток занят вызовом расширения и не может выполнять пользовательский PHP-код heartbeat в том же потоке. Соединение может быть закрыто Oracle, сетевым устройством или из-за сетевого сбоя. Persistent connection сам по себе не гарантирует, что соединение останется рабочим.

**Требование:** расширение должно безопасно обнаруживать нерабочие соединения, не выдавать их повторно из пула, корректно обрабатывать ошибки соединения и документировать, что heartbeat не гарантирует сохранность транзакции.

## Ограничения конкурентного heartbeat

- Не выполнять SQL `PING`/`SELECT 1` на том же Oracle-соединении из фонового потока, пока другой поток выполняет запрос или использует это соединение, если используемый драйвер и API явно не гарантируют такую конкурентность. По умолчанию считать одновременное использование одного соединения небезопасным.
- Отдельное контрольное соединение может подтвердить доступность сервера в целом, но не доказывает, что основное соединение и его транзакция всё ещё действительны.
- Фоновый Rust-поток может выполнять мониторинг, но должен соблюдать жизненный цикл PHP-процесса, корректно завершаться при shutdown и не обращаться к PHP API из фонового потока без гарантий Zend Engine. Не запускать один поток на каждое соединение без лимитов и измерений нагрузки.
- TCP keepalive и Oracle Net Dead Connection Detection — сетевые механизмы обнаружения разрыва, а не механизм сохранения активной транзакции и не защита от всех серверных политик завершения сессий.

## Защита на стороне Oracle и сети

Документировать для администратора БД и эксплуатации:

- Проверить применяемые Oracle профили и лимиты сессий, включая `IDLE_TIME`, если он используется.
- Проверить настройки firewall, NAT, балансировщика и ОС на тайм-ауты неактивных TCP-соединений.
- Рассмотреть настройку Oracle Net `SQLNET.EXPIRE_TIME` в `sqlnet.ora` и TCP keepalive согласно документации Oracle и политике эксплуатации. Не навязывать универсальное значение: подобрать интервал под окружение и нагрузку.
- Явно указать, что `SQLNET.EXPIRE_TIME` помогает обнаруживать мёртвые соединения, но не гарантирует, что сессия не будет завершена другими механизмами.

## Поведение пула и драйвера

1. Перед выдачей соединения из пула выполнить поддерживаемую драйвером проверку валидности, если такая проверка безопасна и доступна. Учитывать, что результат проверки не исключает последующий обрыв.
2. При сетевых ошибках и ошибках Oracle, указывающих на потерю соединения, пометить соединение как сломанное и не возвращать его в пул для повторного использования.
3. Если сбой происходит во время транзакции, сообщить приложению об ошибке; не обещать автоматическое восстановление незакоммиченных изменений.
4. Не выполнять автоматический повтор произвольного SQL после потери соединения: результат операции может быть неизвестен клиенту, а повтор способен привести к дублированию изменений. Повтор допускается только для операций, признанных безопасными/идемпотентными, с явной политикой приложения.
5. При возврате соединения в пул выполнять сброс состояния по безопасному контракту: завершить/откатить незавершённую транзакцию, очистить состояние сессии в поддерживаемом объёме; при неудаче сброса закрыть соединение вместо повторного использования.
6. Добавить метрики и логи: ошибки проверки соединения, обнаруженные мёртвые соединения, удаление из пула, ошибки сброса, переподключения и длительность запросов. Не писать пароли, токены и чувствительные SQL-данные в логи.

## Настройки расширения

Предусмотреть документированные настройки с безопасными значениями по умолчанию и проверкой допустимых диапазонов, например:

```ini
; Проверять соединение перед выдачей из пула (если драйвер поддерживает проверку)
rs_database_connect.validate_on_checkout = On

; Не выдавать повторно соединение после ошибки, классифицированной как потеря связи
rs_database_connect.discard_broken_connections = On
```

Это предлагаемые имена для спецификации, а не утверждение, что параметры уже реализованы. Не добавлять пользовательский SQL heartbeat на занятое соединение как универсальный механизм.

## Тесты

- Соединение разрывается до получения из пула: соединение не выдаётся как валидное; пул создаёт новое либо возвращает понятную ошибку.
- Сеть/сервер обрывает соединение во время длительного запроса: ошибка корректно передаётся PHP; соединение помечается сломанным.
- Потеря соединения во время транзакции: расширение не заявляет, что транзакция сохранена или что результат операции известен.
- Соединение не проходит сброс состояния при возврате в пул: соединение закрывается и не используется следующим запросом.
- Длительная операция не вызывает конкурентный SQL ping на том же соединении.
- Отдельное контрольное соединение, если реализовано, не считается подтверждением валидности основного соединения.
- Провести интеграционные тесты с реальным Oracle/Instant Client и тестовую имитацию сетевого обрыва; не считать unit-тесты достаточными для сетевых гарантий.

## Критерий приёмки

Расширение не обещает предотвратить любой тайм-аут Oracle или сети. Оно должно минимизировать повторное использование сломанных соединений, безопасно сообщать о потере связи и чётко документировать границы гарантий. Защита от idle/network timeout должна включать согласованные настройки Oracle Net, ОС и промежуточной сети; один PHP heartbeat не является достаточным решением.

## Уточнение: Oracle DRCP `INACTIVITY_TIMEOUT`, `MAX_THINK_TIME` и интервалы в секундах

`DBA_CPOOL_INFO` описывает серверный пул DRCP. Эти таймеры нельзя считать взаимозаменяемыми:

- `INACTIVITY_TIMEOUT` — сколько свободный сервер может простаивать внутри DRCP-пула до завершения; этот параметр сам по себе не является таймером heartbeat клиентского соединения, пока сервер выдан клиенту.
- `MAX_THINK_TIME` — максимальный интервал без database call после того, как клиент получил pooled server. По истечении времени сервер может быть возвращён в пул, а клиентское соединение завершено.
- `MAX_TXN_THINK_TIME` (если поддерживается используемой версией Oracle) задаёт отдельный лимит бездействия для клиента с открытой транзакцией; превышение может привести к rollback и завершению клиентского соединения.

Источник для значений и семантики: Oracle Database Reference, `DBA_CPOOL_INFO` и документация `DBMS_CONNECTION_POOL`.

### Требования к настройкам расширения

Все интервалы heartbeat и тайм-ауты, задаваемые в `php.ini`, должны иметь суффикс `_seconds` и измеряться в **секундах**. Не путать эти настройки расширения с параметрами, которыми администратор управляет на стороне Oracle DRCP.

Предлагаемые настройки спецификации (требуют реализации и проверки на выбранном Oracle/ODPI-C стеке):

```ini
; Включить heartbeat только если драйвер допускает безопасный доступ к соединению
; из фонового исполнителя и соединение в данный момент не выполняет DB-вызов.
rs_database_connect.oracle_heartbeat_enabled = Off

; Период heartbeat в секундах; должен быть меньше MAX_THINK_TIME
; с запасом на задержки планировщика, сети и нагрузки.
rs_database_connect.oracle_heartbeat_interval_seconds = 30

; Остановить heartbeat и ждать завершения фоновой проверки при освобождении/закрытии соединения.
rs_database_connect.oracle_heartbeat_shutdown_timeout_seconds = 5
```

**Важные ограничения реализации:**

1. `30` секунд — пример, не универсальное значение. Интервал следует выбирать заметно меньше фактического `MAX_THINK_TIME`; если лимит Oracle равен 120 секундам, интервал в 30 секунд оставляет запас, но его достаточность нужно проверить нагрузочными тестами.
2. Не запускать heartbeat одновременно с запросом на том же соединении. Фоновая проверка разрешена только если конкретный драйвер и обёртка обеспечивают синхронизацию доступа к OCI/ODPI-C и гарантируют, что соединение свободно. Если это нельзя доказать и протестировать, параметр heartbeat должен оставаться выключенным.
3. Отдельное контрольное соединение не сбрасывает `MAX_THINK_TIME` исходного соединения, поэтому оно не может служить заменой heartbeat исходного DRCP-клиента.
4. `INACTIVITY_TIMEOUT` не следует «обходить» ping-ом от PHP: он управляет свободными серверами в пуле. Если сервер свободен и таймер истёк, его удаление является штатным поведением пула.
5. Heartbeat не должен коммитить или откатывать транзакцию. Проверка должна использовать безопасный database call и сохранять исходное состояние сессии; если это невозможно гарантировать, использовать более высокий `MAX_THINK_TIME`/`MAX_TXN_THINK_TIME` на стороне DBA либо не удерживать DRCP-соединение во время длительной CPU-операции.
6. Параметр `oracle_heartbeat_interval_seconds` не должен автоматически менять настройки Oracle. Значения `MAX_THINK_TIME`, `MAX_TXN_THINK_TIME` и `INACTIVITY_TIMEOUT` задаются и проверяются администратором БД отдельно.

### Дополнительные тесты приёмки

- Для заданного `MAX_THINK_TIME` проверить, что heartbeat исходного соединения происходит чаще этого лимита с настроенным запасом.
- Имитировать длительную CPU-операцию PHP без DB-вызовов и подтвердить, что heartbeat работает только при безопасно освобождённом соединении.
- Имитировать длительный DB-вызов и подтвердить, что heartbeat не обращается к тому же соединению параллельно.
- Проверить транзакцию с открытыми изменениями: heartbeat не выполняет commit/rollback и не меняет пользовательское состояние сессии.
- Проверить завершение PHP-процесса и закрытие соединения: фоновая задача останавливается в пределах `oracle_heartbeat_shutdown_timeout_seconds` либо безопасно отменяется согласно контракту драйвера.


---

# Дополнение: проектирование Oracle DRCP heartbeat на Rust (toolchain `rustc 1.99.1`)

## Ключевое решение

Нельзя просто запустить `thread::spawn(|| SELECT 1 FROM DUAL)` с копией указателя на Oracle connection. Указатель на C/OCI handle нельзя считать безопасным для передачи между потоками, а конкурентные вызовы по одному соединению могут повредить протокол или состояние statement/транзакции.

Heartbeat нужен для окна, когда PHP выполняет CPU-работу и соединение не занято SQL. Поэтому все обращения к одному соединению должны проходить через один общий арбитр/lock. SQL-операция держит lock всё время работы с драйвером; worker использует `try_lock()` и пропускает цикл, если соединение занято. Это предотвращает параллельный ping, но не позволяет ping прервать длительный SQL-вызов. Во время CPU-работы, если основной поток не удерживает lock, worker может выполнить короткий ping.

**Это только схема синхронизации.** Реальная реализация разрешена лишь если используемый Oracle-драйвер и его API документированно допускают выполнение операций с connection handle из фонового потока (или handle создан/инициализирован в подходящем threaded mode). Если драйвер не может быть безопасно передан между потоками (`Send`/`Sync` не поддерживается обоснованно), heartbeat на том же соединении не реализовывать. Отдельное соединение не сбрасывает `MAX_THINK_TIME` исходного соединения.

## Минимальный учебный Rust-пример

Пример показывает только владение, остановку worker и эксклюзивный доступ. `OracleConnection` ниже — заглушка-протокол, не реальный API crate `oracle`/ODPI-C. Его нельзя подключать к production без реализации `ping()` через поддерживаемый API драйвера и подтверждения thread-safety.

```rust
use std::sync::{mpsc, Arc, Mutex, TryLockError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

// Демонстрационный интерфейс. Реальная реализация должна быть основана
// на документированном API выбранного Oracle-драйвера.
trait PingConnection: Send + 'static {
    type Error: std::fmt::Display;
    fn ping(&mut self) -> Result<(), Self::Error>;
}

struct Heartbeat<C: PingConnection> {
    conn: Arc<Mutex<C>>,
    stop_tx: Option<mpsc::Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl<C: PingConnection> Heartbeat<C> {
    fn start(conn: Arc<Mutex<C>>, interval: Duration) -> Self {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let worker_conn = Arc::clone(&conn);

        let worker = thread::Builder::new()
            .name("oracle-drcp-heartbeat".into())
            .spawn(move || loop {
                // Остановка с ожиданием интервала, а не безусловный sleep,
                // чтобы worker мог быстро заметить сигнал остановки.
                match stop_rx.recv_timeout(interval) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }

                // Не ждать за занятым соединением и никогда не выполнять
                // ping параллельно с основным SQL-вызовом.
                let mut guard = match worker_conn.try_lock() {
                    Ok(guard) => guard,
                    Err(TryLockError::WouldBlock) => continue,
                    Err(TryLockError::Poisoned(_)) => break,
                };

                // Обязателен ограниченный сетевой/операционный timeout внутри
                // драйвера: остановить зависший native call извне надёжно нельзя.
                if let Err(err) = guard.ping() {
                    eprintln!("Oracle heartbeat failed: {err}");
                    // Production policy: пометить lease как broken через
                    // отдельный канал состояния; не переиспользовать соединение.
                    break;
                }
            })
            .expect("failed to start heartbeat thread");

        Self { conn, stop_tx: Some(stop_tx), worker: Some(worker) }
    }

    // Все рабочие операции с этим соединением тоже должны брать этот mutex.
    fn with_connection<R>(&self, f: impl FnOnce(&mut C) -> R) -> R {
        let mut guard = self.conn.lock().expect("connection mutex poisoned");
        f(&mut *guard)
    }

    fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(worker) = self.worker.take() {
            // Join может ждать, пока ping вернётся. Поэтому ping обязан иметь
            // конечный timeout; не блокировать здесь PHP shutdown бесконечно.
            let _ = worker.join();
        }
    }
}

impl<C: PingConnection> Drop for Heartbeat<C> {
    fn drop(&mut self) {
        self.stop();
    }
}
```

### Что нужно изменить перед production

1. Не использовать `eprintln!` в расширении: отправлять диагностические события в ограниченную очередь/счётчики, не раскрывать DSN/пароли/SQL с секретами.
2. Не использовать `expect()` в lifecycle расширения: возвращать ошибки запуска потока как исключение и корректно откатывать частично созданные ресурсы.
3. Не полагаться на `Drop` как на единственный механизм завершения. Явно останавливать worker при закрытии lease, `RSHUTDOWN`, `MSHUTDOWN` и ошибке инициализации; не вызывать PHP/Zend API из worker.
4. Привязать состояние worker к владельцу соединения/lease, а не к PHP-объекту, который может быть уничтожен в другом месте. Убедиться, что не происходит `fork()` после создания worker/соединения; в дочернем процессе унаследованные соединения не использовать.
5. Использовать `try_lock()` только если все SQL-вызовы и смены состояния сессии используют тот же lock. Иначе lock не предотвращает конкурентное использование.
6. Обеспечить конечный timeout для ping и всех операций драйвера. `std::thread::JoinHandle::join()` не имеет встроенного timeout и ждёт завершения потока.
7. Определить политику ошибки: worker сообщает `Broken` через атомарное состояние/канал; PHP-поток при следующем безопасном обращении выбрасывает исключение и удаляет соединение из пула. Не делать автоматический повтор неидемпотентного SQL.
8. `interval_seconds` должен быть положительным и с запасом меньше серверного `MAX_THINK_TIME`; это не гарантирует успех при паузах планировщика, перегрузке или зависшем сетевом вызове.
9. Для транзакции с открытыми изменениями проверять фактическое поведение DRCP на целевой версии Oracle, включая `MAX_TXN_THINK_TIME`. Не утверждать, что heartbeat сохраняет транзакцию при разрыве.
10. Прежде чем добавлять зависимость/код, проверить `rustc --version`, `cargo --version`, `cargo tree -i oracle`, версии crate `oracle` и ODPI-C/Instant Client, а также режим и гарантии thread-safety в документации upstream. Закрепить toolchain и `Cargo.lock` в CI.

## Предлагаемые параметры INI

Все значения ниже — в секундах; это параметры расширения, они не изменяют настройки Oracle:

```ini
; Выключено до подтверждения поддержки конкретного драйвера
rs_database_connect.oracle_heartbeat_enabled = Off
rs_database_connect.oracle_heartbeat_interval_seconds = 30
rs_database_connect.oracle_heartbeat_timeout_seconds = 5
```

`oracle_heartbeat_timeout_seconds` должен быть реализован через timeout самого драйвера/сокета, если это возможно. Он не может принудительно безопасно прервать произвольный зависший native OCI call. Если конечный timeout обеспечить нельзя, heartbeat-worker не включать.

## План реализации по этапам

1. **Без потоков:** добавить диагностику фактических ошибок DRCP, idle validation на checkout, исключение broken connections из пула и интеграционный тест `MAX_THINK_TIME`.
2. **Proof of concept вне PHP:** проверить в отдельной Rust-программе, что выбранный Oracle API поддерживает требуемую модель потоков, mutex и ping на одном соединении; провести тест CPU-idle и занятого SQL.
3. **Thread/lifecycle review:** проверить NTS/ZTS PHP, shutdown, fatal error, fork, длительный native call, обработку panic и утечки.
4. **Только затем:** реализовать опциональный INI-флаг, выключенный по умолчанию, и включать его в production явно после испытаний.
5. **Если драйвер не гарантирует безопасный доступ:** не делать heartbeat на основном соединении; согласовать с DBA увеличение `MAX_THINK_TIME`/`MAX_TXN_THINK_TIME` или менять модель работы так, чтобы не удерживать DRCP-соединение во время долгой CPU-операции.

## Приёмочные тесты

- Во время CPU-bound участка без DB-вызовов heartbeat может получить lock и выполнить ping на исходном соединении.
- Во время SQL-вызова heartbeat не входит в connection concurrently; цикл пропускается, а не ставится в очередь на неопределённое время.
- Ошибка ping маркирует соединение сломанным и не приводит к автоматическому повтору последнего SQL.
- Worker завершается при штатном shutdown и не вызывает PHP API.
- Зависший ping ограничен timeout драйвера; shutdown не блокируется бесконечно.
- Транзакция с открытыми изменениями проверяется на настоящем DRCP; любые потери связи приводят к понятной ошибке, а не к ложному обещанию сохранности транзакции.
- Сборка и тесты запускаются закреплённым toolchain `rustc 1.99.1 (58cf8f9c3 2026-10-13)` и проходят для целевых платформ/PHP ABI.
