# VeloGate

VeloGate is a domain-specific compiler and lightweight asynchronous runtime prototype written in Rust. It lets you describe API gateways and BFF endpoints declaratively, infer backend request dependencies, build a DAG execution plan, and configure security and resilience policies without writing imperative orchestration code.

The project currently focuses on the compiler pipeline:

- a `.gate` DSL lexer and parser;
- AST construction with interned symbols;
- syntax diagnostics rendered with `ariadne`;
- semantic planning with a dependency DAG;
- parallel execution layers for independent backend calls;
- AST, JSON, DOT graph, and execution-plan exports.

## Why

Typical BFF code mixes endpoint shape, backend calls, retries, fallbacks, rate limits, authentication, and response mapping in imperative handlers. VeloGate moves that into a declarative DSL and lets the compiler decide what can run in parallel.

For example, if `weather` does not depend on `user`, both requests can be placed in the same execution layer. If `raw_orders` uses `user.id`, it is scheduled after `user`.

## DSL Example

See [examples/main.gate](examples/main.gate):

```gate
gateway "VeloGate-BFF" {
    port: 8080,
    host: "0.0.0.0"
}

endpoint "GET /api/v1/dashboard" {
    secure: [BearerJWT],
    rate_limit: 100/rps window 1s,

    fetch "http://users-service/me" as user {
        timeout: 100ms,
        fallback: { "id": 0, "role": "guest", "name": "Anonymous" }
    };

    fetch "http://weather-service/current" as weather {
        timeout: 50ms,
        fallback: { "temp": 20, "condition": "unknown" }
    };

    fetch "http://orders-service/list?user_id=" + user.id as raw_orders {
        timeout: 500ms,
        retry: 2 times,
        delay: 10ms,
        fallback: { "orders": [] }
    };

    let top_orders = raw_orders.orders
        | filter(order => order.status == "completed" || order.total > 500)
        | map(order => {
            "id": order.uuid,
            "amount_usd": order.total / 90.0,
            "items_count": order.items.len()
          })
        | take(3);

    respond 200 {
        "user_name": user.name,
        "is_admin": user.role == "admin",
        "weather": {
            "celsius": weather.temp,
            "status": weather.condition
        },
        "latest_orders": top_orders
    }
}
```

## DAG Planning

VeloGate builds an execution plan from endpoint steps. Each step declares one produced variable, and expressions inside later steps declare dependencies by reading variables.

For the example above:

```text
layer 0: user, weather
layer 1: raw_orders
layer 2: top_orders
```

This means `user` and `weather` can run concurrently. `raw_orders` waits for `user`, and `top_orders` waits for `raw_orders`.

The planner also validates:

- duplicate produced variables;
- undefined variable reads;
- cyclic dependency graphs.

## Install And Build

Requirements:

- Rust stable with Cargo.

Build:

```powershell
cargo build
```

Run tests:

```powershell
cargo test
```

Run clippy:

```powershell
cargo clippy --all-targets --all-features -- -D warnings
```

## CLI

```text
Usage: velogate <COMMAND>

Commands:
  check  Validate a .gate file without starting the server
  start  Start the API gateway runtime
  dump   Export internal representation
```

Validate syntax and semantic execution plan:

```powershell
cargo run -- check --config .\examples\main.gate
```

Export readable AST:

```powershell
cargo run -- dump --config .\examples\main.gate --format ast
```

Export JSON AST:

```powershell
cargo run -- dump --config .\examples\main.gate --format json
```

Export execution plan:

```powershell
cargo run -- dump --config .\examples\main.gate --format plan
```

Export DOT graph:

```powershell
cargo run -- dump --config .\examples\main.gate --format graph
```

## Current Status

Implemented:

- parsing gateway and endpoint declarations;
- endpoint options: `secure`, `rate_limit`;
- fetch steps with timeout, retry, delay, fallback;
- object and array literals;
- property access and method calls;
- filter/map/take pipelines;
- dependency DAG planning;
- plan and graph export;
- syntax and planning validation.

In progress / next steps:

- actual HTTP runtime executor from `ExecutionPlan`;
- request context and response value model;
- auth middleware generation from `secure`;
- rate-limit middleware generation;
- typed fallback validation;
- better source spans for semantic planner errors.

## Project Goal

The long-term goal is to compile declarative BFF/API gateway descriptions into a fast async runtime plan:

- no hand-written orchestration for independent backend calls;
- predictable dependency scheduling;
- explicit resilience policies;
- simple exportable plans for debugging and optimization.
