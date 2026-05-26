# VeloGate

**VeloGate** — это декларативный шлюз API и высокопроизводительный движок для создания BFF (Backend For Frontend). Он компилирует конфигурационные `.gate` файлы в оптимизированный асинхронный план выполнения, автоматически распределяя независимые запросы по параллельным потокам.

Вам больше не нужно вручную кодить оркестрацию микросервисов, настраивать ретраи, таймауты и склеивать JSON-ответы в императивном коде. Вы описываете результат, а компилятор решает, как выполнить его максимально быстро.

---

## ⚡ Киллер-фичи

* **Авто-параллелизм (Auto-DAG):** Компилятор анализирует зависимости между переменными и автоматически группирует независимые запросы в параллельные слои выполнения.
* **Сквозная отказоустойчивость (Resilience):** Настройка таймаутов, количества повторов (retry), пауз между ними (delay) и дефолтных ответов (fallback) прямо в теле запроса.
* **Встроенная безопасность (Zero-Code Security):** Декларативная валидация JWT-токенов и ролей, а также встроенный ограничитель частоты запросов (Rate Limiting).
* **Пайплайны трансформации данных:** Легковесная и быстрая конвейерная обработка массивов (`filter`, `map`, `take`) без бойлерплейта и лишних циклов.
* **Многодоменный рантайм:** Нативная интеграция с **gRPC (Protobuf)** микросервисами и прямые асинхронные запросы в **Базы Данных (SQL)** на уровне примитивов языка.
* **Визуализация и валидация:** Полная проверка на дубликаты, циклические зависимости и неизвестные переменные **до** запуска сервера с возможностью экспорта графа выполнения в формат Graphviz (DOT).

---

## 📝 Пример синтаксиса (`main.gate`)

```rust
gateway "VeloGate-BFF" {
    port: 8080,
    host: "0.0.0.0",
    
    // Подключение статических баз данных
    databases: [
        Postgres "main_db" { url: "postgres://user:pass@localhost:5432/prod" }
    ]
}

endpoint "GET /api/v1/dashboard" {
    secure: [BearerJWT, Roles(["admin"])],
    rate_limit: 100/rps window 1s,

    // Эти два запроса запустятся ОДНОВРЕМЕННО (Слой 0)
    fetch "http://users-service/me" as user {
        timeout: 100ms,
        fallback: { "id": 0, "role": "guest", "name": "Anonymous" }
    };

    fetch "http://weather-service/current" as weather {
        timeout: 50ms,
        fallback: { "temp": 20, "condition": "unknown" }
    };

    // Быстрая проверка прав прямо в БД перед тяжелыми запросами
    let ban_status = db::query("main_db", "SELECT is_banned FROM blacklist WHERE user_id = $1", user.id)
        timeout 50ms;

    // Запрос выполнится строго после получения user.id (Слой 1)
    fetch "http://orders-service/list?user_id=" + user.id as raw_orders {
        timeout: 500ms,
        retry: 2 times,
        delay: 10ms,
        fallback: { "orders": [] }
    };

    // Трансформация и фильтрация массива данных "на лету" (Слой 2)
    let top_orders = raw_orders.orders
        | filter(order => order.status == "completed" || order.total > 500)
        | map(order => {
            "id": order.uuid,
            "amount_usd": order.total / 90.0,
            "items_count": order.items.len()
          })
        | take(3);

    // Сборка идеального ответа для фронтенда
    respond 200 {
        "user_name": user.name,
        "is_banned": ban_status.is_banned,
        "weather": {
            "celsius": weather.temp,
            "status": weather.condition
        },
        "latest_orders": top_orders
    }
}
```

---

## 🗺️ Как это работает (План слоев)

Для примера выше компилятор VeloGate автоматически выстроит следующую схему выполнения:

```text
layer 0: user, weather
layer 1: ban_status, raw_orders
layer 2: top_orders
```

---

## 🛠️ Руководство по использованию CLI

Утилита работает в трех режимах: проверка, запуск и экспорт отладочной информации.

### 1. Проверка конфигурации (Линтер)
Проверяет `.gate` файл на синтаксические ошибки, опечатки в переменных и циклические зависимости без запуска сервера. В случае ошибки выводит красивый подсвеченный отчет.
```powershell
velogate check --config ./main.gate
```

### 2. Запуск шлюза в продакшн
Запускает высокопроизводительный веб-сервер и открывает порты. Можно явно указать количество выделенных процессорных потоков.
```powershell
velogate start --config ./main.gate --workers 4
```

### 3. Экспорт внутренних данных (Dump)
Помогает понять, как компилятор видит ваш код, и экспортирует его в разные форматы.

* **Просмотр AST-дерева:**
  ```powershell
  velogate dump --config ./main.gate --format ast
  ```
* **Экспорт плана выполнения слоев в JSON:**
  ```powershell
  velogate dump --config ./main.gate --format plan
  ```
* **Генерация DOT-графа для визуализации в Graphviz/Mermaid:**
  ```powershell
  velogate dump --config ./main.gate --format graph
  ```

---

## 🚀 Быстрый старт

### Сборка проекта
Для сборки вам понадобится установленный компилятор Rust (Stable).
```powershell
cargo build --release
```

### Запуск тестов
```powershell
cargo test
```