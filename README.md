# VeloGate

VeloGate - декларативный API gateway/BFF runtime. Он читает `.gate` конфигурацию, строит план выполнения endpoint-а, проверяет зависимости между переменными и запускает HTTP runtime на Axum.

Основная идея: описать входные ручки, внешние HTTP/gRPC/DB вызовы, security, retry/fallback и трансформации JSON в одном `.gate` файле.

## Возможности

- HTTP endpoint-ы: `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, `OPTIONS`, `TRACE`.
- Чтение входящего request context: path params, query, headers, cookies, body.
- Исходящие HTTP `fetch` с настраиваемым `method`, JSON `body`, `timeout`, `retry`, `delay`, `fallback`.
- JWT security по секрету HS256 и custom checks.
- Basic authorization и custom checks.
- `gateway.env_file` и `gateway.constants`, доступные всем endpoint-ам.
- Pipe-операции над массивами: `filter`, `map`, `take`.
- `take(...)` принимает выражение, например `take(default_take)`.
- Built-in функции и методы для строк, массивов, проверок и форматирования.
- SQL запросы через `db::query`.
- gRPC unary calls, включая protobuf reflection через `.proto`.
- Semantic validator до построения плана: обязательный порт, диапазоны статусов, HTTP methods, rate-limit units, дубликаты routes/constants/db/proto и существование env/proto файлов.
- Planner проверяет неизвестные переменные, дубликаты и зависимости до старта runtime.
- Dump/export AST, plan и graph.
- CLI-команды `fmt`, `lint`, `routes`, `doctor`.
- Structured logs через `tracing`.

## Быстрый пример

```rust
gateway "VeloGate-BFF" {
    port: 8080,
    host: "0.0.0.0",
    env_file: ".env",
    constants: {
        "todos_api": env.TODOS_API,
        "default_take": 100,
        "service_name": "todos-bff"
    }
}

endpoint "GET /api/v1/todos" {
    rate_limit: 100/rps window 1s,

    fetch todos_api + "/todos" as raw_todos {
        timeout: 500ms,
        retry: 2 times,
        delay: 10ms,
        fallback: []
    };

    let completed_todos = raw_todos
        | filter(t => t.id > 10 && t.id < 20)
        | map(t => {
            "id": t.id,
            "user_id": t.userId,
            "title": t.title
        })
        | take(default_take);

    respond 200 {
        "service": service_name,
        "data": completed_todos
    }
}
```

## Gateway

`gateway` задает имя, порт, host, глобальные переменные, env-файл, базы данных и proto-файлы.

```rust
gateway "api" {
    port: 8080,
    host: "0.0.0.0",
    env_file: ".env",

    constants: {
        "api_base": env.API_BASE,
        "default_take": 20,
        "service_name": "public-api"
    },

    databases: [
        sqlite "main" { url: "sqlite::memory:" }
    ],

    protos: [
        proto "profile_proto" { path: "./proto/profile.proto" }
    ]
}
```

`env_file` резолвится относительно `.gate` файла при запуске через CLI. Значения из файла доступны через объект `env`:

```env
API_BASE=https://api.example.test
JWT_SECRET=dev-secret
ADMIN_PASSWORD=secret
```

`constants` доступны всем endpoint-ам как обычные переменные:

```rust
fetch api_base + "/users" as users;
...
| take(default_take);
```

## Endpoint И Request Context

Endpoint задается строкой `"METHOD /path/:param"`:

```rust
endpoint "PATCH /api/v1/todos/:id" {
    respond 200 {
        "id": id,
        "query": query,
        "user_agent": headers.user_agent,
        "session": cookies.session,
        "body": body
    }
}
```

Доступные встроенные переменные:

- path params: `id`, `user_id`, любые `:param` из path.
- `query` - query string как объект.
- `headers` - headers как объект, `-` заменяется на `_`, например `x-trace-id` становится `headers.x_trace_id`.
- `cookies` - cookies как объект.
- `body` - JSON тело запроса; если тело не JSON, будет строка; если пустое, `null`.
- `env` - значения из `gateway.env_file`.
- `jwt` - claims после успешной JWT авторизации.
- `basic` - объект Basic auth после успешной Basic авторизации.

## Исходящие HTTP Запросы

По умолчанию `fetch` делает `GET`:

```rust
fetch "https://example.test/users" as users {
    timeout: 500ms,
    retry: 2 times,
    delay: 20ms,
    fallback: []
};
```

Можно указать метод и тело:

```rust
endpoint "POST /api/v1/todos" {
    fetch todos_api + "/todos" as created_todo {
        method: "POST",
        body: {
            "title": body.title,
            "completed": body.completed,
            "userId": body.userId
        },
        timeout: 500ms,
        fallback: {
            "id": 0,
            "title": body.title
        }
    };

    respond 201 {
        "created": created_todo
    }
}
```

Поддерживаются любые валидные HTTP methods, включая `POST`, `PUT`, `PATCH`, `DELETE`.

## Security

### JWT

JWT проверяется по `Authorization: Bearer <token>` и secret. Сейчас используется HS256.

```rust
endpoint "GET /api/v1/me/:id" {
    secure: [
        JWT {
            secret: env.JWT_SECRET,
            checks: [
                jwt.sub == id,
                jwt.role == "admin" || jwt.role == "user"
            ]
        }
    ],

    respond 200 {
        "user_id": jwt.sub,
        "role": jwt.role,
        "claims": jwt
    }
}
```

`checks` - список выражений. Все выражения должны быть truthy, иначе ответ будет `401`.

### Basic

Basic auth проверяется по `Authorization: Basic base64(username:password)`.

```rust
endpoint "GET /api/v1/admin" {
    secure: [
        Basic {
            username: "admin",
            password: env.ADMIN_PASSWORD,
            checks: [
                basic.username == "admin"
            ]
        }
    ],

    respond 200 {
        "ok": true,
        "user": basic.username
    }
}
```

`username`, `password`, `secret` могут быть выражениями, поэтому их можно брать из `env` или `constants`.

Старый `BearerJWT` больше не поддерживается.

## Rate Limit

```rust
endpoint "GET /api/v1/todos" {
    rate_limit: 100/rps window 1s,
    respond 200 { "ok": true }
}
```

Rate limit считается по IP клиента. Runtime использует первый доступный источник:

- `x-forwarded-for`;
- `x-real-ip`;
- socket address из Axum `ConnectInfo`;
- `unknown`, если IP определить нельзя.

Если лимит превышен, runtime возвращает `429`.

## Respond

Короткая форма возвращает JSON-объект:

```rust
respond 200 {
    "ok": true,
    "data": users
}
```

Расширенная форма позволяет вернуть тело, headers и cookies в любой нужной комбинации:

```rust
respond 200
    headers {
        "x-trace-id": headers.x_trace_id,
        "cache-control": "no-store"
    }
    cookies {
        "session": token
    }
    body {
        "ok": true,
        "data": users
    }
```

`body` всегда является словарем. Можно вернуть только статус без тела:

```rust
respond 204
```

Можно вернуть только тело и headers, только тело и cookies, только cookies/headers, или все вместе.

## Pipe: filter, map, take

```rust
let visible = users
    | filter(user => contains(allowed_roles, user.role) && user.name.trim().lower().contains("a"))
    | map(user => {
        "name": user.name.trim(),
        "label": format("{}:{}", user.role.upper(), user.name.trim())
    })
    | take(default_take);
```

`filter` принимает условие. `map` строит новый объект. `take` принимает любое выражение, которое вычисляется в неотрицательное целое число.

Planner учитывает переменные внутри `filter`, `map`, `take`, secure checks, response и fetch config. Например `take(default_take)` корректно зависит от `default_take`.

## Built-in Функции И Методы

Функции:

```rust
len(value)
contains(container, value)
starts_with(text, prefix)
ends_with(text, suffix)
lower(text)
upper(text)
trim(text)
replace(text, from, to)
split(text, separator)
join(array, separator)
format("hello {}", name)
string(value)
number(value)
bool(value)
is_null(value)
is_empty(value)
```

Методы:

```rust
value.len()
text.contains("abc")
text.starts_with("a")
text.ends_with("z")
text.lower()
text.upper()
text.trim()
text.replace("from", "to")
text.split(",")
array.join(",")
```

Примеры:

```rust
respond 200 {
    "allowed": contains(["admin", "ops"], jwt.role),
    "name": body.name.trim().upper(),
    "label": format("{}:{}", jwt.role, body.name.trim()),
    "tags": "a,b,c".split(",")
}
```

## Expressions

Поддерживаются:

- строки, числа, bool, массивы, объекты;
- переменные;
- property access: `user.name`;
- function calls и method calls;
- арифметика: `+`, `-`, `*`, `/`, `%`;
- сравнения: `==`, `!=`, `>`, `<`, `>=`, `<=`;
- boolean logic: `&&`, `||`.

Оператор `+` складывает числа, а если один из операндов строка, выполняет конкатенацию.

## Database

```rust
gateway "api" {
    port: 8080,
    databases: [
        sqlite "main" { url: "sqlite::memory:" }
    ]
}

endpoint "GET /db" {
    let rows = db::query("main", "select ? as answer, ? as label", 42, "ok");

    respond 200 {
        "rows": rows
    }
}
```

Можно указывать timeout/fallback:

```rust
let rows = db::query("main", "select * from users where id = ?", id) {
    timeout: 100ms,
    fallback: []
};
```

## gRPC

Простой вызов через `google.protobuf.Struct`:

```rust
let profile = grpc::call(
    "http://profiles:50051/profile.Profile/Get",
    { "id": id }
);
```

Вызов с `.proto`:

```rust
gateway "api" {
    port: 8080,
    protos: [
        proto "profile_proto" { path: "./proto/profile.proto" }
    ]
}

endpoint "GET /profile/:id" {
    let profile = grpc::call(
        "http://profiles:50051",
        "profile_proto",
        "profile.Profile",
        "Get",
        { "id": id }
    );

    respond 200 {
        "profile": profile
    }
}
```

## План Выполнения

VeloGate строит DAG зависимостей между step-ами endpoint-а. Независимые step-ы выполняются в одном слое параллельно.

Пример:

```rust
endpoint "GET /dashboard" {
    fetch users_url as user;
    fetch weather_url as weather;
    fetch orders_url + "?user_id=" + user.id as orders;

    respond 200 {
        "user": user,
        "weather": weather,
        "orders": orders
    }
}
```

План:

```text
layer 0: user, weather
layer 1: orders
```

Force sequential execution with `sync`:

```rust
endpoint "GET /ops/health" {
    command "echo before" as before;

    sync {
        command "echo first" as first;
        command "echo second" as second;
    }

    let label = "after-sync";

    respond 200 {
        "before": before.stdout,
        "first": first.stdout,
        "second": second.stdout,
        "label": label
    }
}
```

Plan:

```text
layer 0: before
layer 1: first
layer 2: second
layer 3: label
```

Run a local shell command with `command "..." as name;` or
`let name = command "...";`. The result is an object:

```json
{
  "success": true,
  "status": 0,
  "stdout": "ok",
  "stderr": ""
}
```

Planner также отклоняет:

- дубликаты переменных;
- неизвестные переменные;
- циклические зависимости.

## Validation

После парсинга и до planner-а VeloGate запускает semantic validator. Он отклоняет:

- отсутствующий `gateway.port`;
- `gateway.port` вне диапазона `1..65535`;
- response status вне диапазона `100..599`;
- неподдерживаемые HTTP methods;
- неизвестные `rate_limit` units;
- дубликаты endpoint routes;
- дубликаты gateway constants, databases и protos;
- отсутствующие `gateway.env_file` и `.proto` файлы.

`check`, `lint`, `start`, `dump`, `routes` и `doctor` проходят через этот слой.

`check` отвечает на вопрос "можно ли загрузить и запланировать конфиг". `lint` сначала выполняет тот же строгий pipeline, а затем добавляет предупреждения по качеству конфигурации.

Грамматика DSL строгая для известных config-блоков. Опечатки в ключах gateway и security не интерпретируются как другие поля. Например `envfile`, `constantz` или `secrit` будут syntax error, а не неявный `env_file`, `constants` или `secret`.

## Observability

Runtime использует `tracing`. Сейчас логируются:

- старт runtime и количество worker threads;
- bind address;
- начало обработки request-а на endpoint-е;
- отказ security rule;
- выполнение step-а;
- попытки исходящего HTTP request-а, включая method, url и номер retry;
- runtime errors с endpoint, HTTP status и кодом ошибки.

Уровень логирования можно настраивать через `RUST_LOG`, например:

```powershell
$env:RUST_LOG="velogate=debug,tower_http=info"
cargo run -- start --config examples/main.gate
```

## CLI

Проверить `.gate` файл на синтаксис, semantic validation и execution plan:

```powershell
cargo run -- check --config examples/main.gate
```

Запустить lint. Он не заменяет validator: ошибки по-прежнему останавливают команду, а warnings выводятся отдельно и не делают конфиг невалидным.

```powershell
cargo run -- lint --config examples/main.gate
```

Сейчас lint предупреждает о:

- `public_bind` - gateway слушает `0.0.0.0`;
- `public_endpoint` - endpoint без `secure`;
- `unused_constant` - gateway constant не используется endpoint-ами;
- `unused_database` - database объявлена, но не используется;
- `unused_proto` - proto объявлен, но не используется;
- `fetch_without_timeout` - исходящий HTTP fetch без timeout;
- `retry_without_delay` - retry без delay;
- `fallback_without_retry` - fallback без retry;
- `db_without_timeout` - DB query без timeout;
- `grpc_without_timeout` - gRPC call без timeout;
- `fallback_without_timeout` - fallback на gRPC call без timeout.

Пример:

```text
lint warning [public_bind]: gateway binds to 0.0.0.0; make sure this is intended
lint warning [public_endpoint]: endpoint `GET /api/v1/todos` has no security rule
lint ok: examples/main.gate (6 endpoint(s), 5 planned step(s), 6 warning(s))
```

Нормализовать `.gate` файл. Сейчас formatter сохраняет комментарии и структуру файла, нормализует переводы строк, хвостовые пробелы, повторные пустые строки и финальный newline:

```powershell
cargo run -- fmt --config examples/main.gate
```

Проверить форматирование без записи:

```powershell
cargo run -- fmt --config examples/main.gate --check
```

Показать routes, auth, rate limits, step dependencies и execution layers:

```powershell
cargo run -- routes --config examples/main.gate
```

Пример вывода:

```text
GET /api/v1/todos
  response: 200
  auth: none
  rate_limit: 100/rps window 1000ms by ip
  steps:
    - #0 raw_todos FetchHttp deps: none
    - #1 completed_todos Pipe deps: raw_todos
  layers:
    - layer 0: raw_todos
    - layer 1: completed_todos
```

Проверить общее состояние конфига и резолвинг входных файлов:

```powershell
cargo run -- doctor --config examples/main.gate
```

Запустить gateway:

```powershell
cargo run -- start --config examples/main.gate --workers 4
```

Переопределить env-файл из CLI:

```powershell
cargo run -- start --config examples/main.gate --env-file examples/.env
```

Dump AST:

```powershell
cargo run -- dump --config examples/main.gate --format ast
```

Dump execution plan:

```powershell
cargo run -- dump --config examples/main.gate --format plan
```

Export graph:

```powershell
cargo run -- dump --config examples/main.gate --format graph
```

## Примеры

Основной пример:

- `examples/main.gate`
- `examples/.env`

Отдельные кейсы:

- `examples/cases/01_read_routes.gate` - GET, query/path/headers, pipe.
- `examples/cases/02_write_body.gate` - POST/PATCH и request body.
- `examples/cases/03_jwt_secure.gate` - JWT auth и checks.
- `examples/cases/04_basic_env_constants.gate` - Basic auth, env, constants.
- `examples/cases/05_builtins_and_variable_take.gate` - built-ins и `take(default_take)`.

Проверить все примеры можно через тест:

```powershell
cargo test gate_examples_parse_and_plan
```

## Разработка

Сборка:

```powershell
cargo build
```

Тесты:

```powershell
cargo test
```

Форматирование:

```powershell
cargo fmt
```
