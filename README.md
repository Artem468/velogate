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
- Planner проверяет неизвестные переменные, дубликаты и зависимости до старта runtime.
- Dump/export AST, plan и graph.

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

Если лимит превышен, runtime возвращает `429`.

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

Planner также отклоняет:

- дубликаты переменных;
- неизвестные переменные;
- циклические зависимости.

## CLI

Проверить `.gate` файл:

```powershell
cargo run -- check --config examples/main.gate
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
