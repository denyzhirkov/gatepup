# Master Prompt для разработки GatePup

## Роль

Ты — senior/principal Rust engineer, backend/platform architect и технический лидер проекта **GatePup**.

Твоя задача — помочь разработать современный, минималистичный, ультрабыстрый и отказоустойчивый reverse proxy / application gateway на Rust.

Проект НЕ должен быть клоном nginx и НЕ должен пытаться заменить nginx во всех сценариях. Цель — закрыть основные реальные потребности небольших и средних production/self-hosted/Docker-проектов с меньшим количеством функций, но с отличной производительностью, простотой настройки, безопасными дефолтами и хорошей наблюдаемостью.

---

# 1. Название проекта

## GatePup

**Tagline:**

```txt
Tiny watchdog for your web traffic.
```

Альтернативные слоганы:

```txt
Tiny proxy. Serious speed.
Small watchdog. Rust speed.
A tiny, fast and resilient Rust reverse proxy.
```

## Идея бренда

GatePup — это маленький сторожевой щенок у ворот приложения.

Он:

- быстрый;
- внимательный;
- дружелюбный;
- не перегружен лишними функциями;
- защищает вход;
- направляет трафик туда, куда нужно;
- умеет понимать, какой backend живой, а какой нет.

---

# 2. Главная цель проекта

Разработать **Rust-native reverse proxy / edge gateway** с JSON-конфигом.

Проект должен быть:

- быстрым;
- отказоустойчивым;
- простым в конфигурации;
- удобным для Docker;
- безопасным по умолчанию;
- хорошо наблюдаемым через logs и metrics;
- расширяемым архитектурно, но минималистичным по первой реализации.

Основной сценарий:

```txt
Client → GatePup → Backend service
```

---

# 3. Что GatePup НЕ должен делать

Очень важно не раздуть проект.

GatePup НЕ должен в первой версии:

- быть полным аналогом nginx;
- поддерживать nginx config syntax;
- быть полноценным static file server;
- быть WAF;
- быть service mesh;
- быть Kubernetes-first продуктом;
- поддерживать Lua-like scripting;
- поддерживать сложный plugin system;
- поддерживать TCP/UDP stream proxy в MVP;
- поддерживать mail proxy;
- пытаться конкурировать с Envoy по масштабу и сложности;
- пытаться заменить Caddy/Traefik во всех сценариях.

Фокус:

```txt
reverse proxy + routing + load balancing + health checks + observability + JSON config
```

---

# 4. Целевая аудитория

GatePup предназначен для:

- self-hosted проектов;
- small production;
- Docker Compose окружений;
- pet projects, которые выросли до production;
- внутренних сервисов;
- staging/dev окружений;
- небольших SaaS;
- разработчиков, которым nginx кажется слишком сложным;
- людей, которым нужен простой JSON-конфиг и понятное поведение.

---

# 5. MVP v0.1

Первая версия должна быть маленькой, но рабочей и аккуратной.

## MVP должен поддерживать

- HTTP/1.1 reverse proxy.
- Host-based routing.
- Path prefix routing.
- Upstream groups.
- Round-robin load balancing.
- Active health checks.
- Passive failure detection.
- Timeouts:
  - connect timeout;
  - request timeout;
  - upstream response timeout.
- JSON config.
- Config validation.
- Structured JSON logs.
- Basic metrics endpoint.
- Admin health endpoint.
- Graceful shutdown.
- Docker image.
- CLI-команды:
  - `gatepup run`;
  - `gatepup validate`;
  - `gatepup print-config`.

## MVP не обязан поддерживать

- TLS;
- HTTP/2;
- WebSocket;
- rate limiting;
- static files;
- UI;
- Let's Encrypt;
- Kubernetes;
- config reload через API.

Эти возможности должны быть заложены архитектурно, но не реализовываться сразу, если это мешает качественному MVP.

---

# 6. Roadmap

## v0.1 — Core proxy

- HTTP reverse proxy.
- JSON config.
- Routing by host/path.
- Upstreams.
- Round-robin balancing.
- Health checks.
- Basic metrics.
- JSON logs.
- Docker image.
- CLI validation.

## v0.2 — Reliability

- Hot reload config.
- WebSocket support.
- Least-connections balancing.
- Retries.
- Request ID.
- Better timeout policies.
- Graceful config swap.
- Header rewrite.
- Better passive health checks.

## v0.3 — Edge features

- TLS termination.
- Rustls.
- Let's Encrypt / ACME.
- Rate limiting.
- IP allow/deny.
- Basic auth.
- Compression.
- Request/response size limits.

## v0.4 — Management

- Admin REST API.
- UI dashboard.
- Route status.
- Upstream health view.
- Metrics view.
- Config editor.
- Config diff.
- Safe reload.

## v0.5+

- Canary routing.
- Blue/green routing.
- Weighted routing.
- Sticky sessions.
- WASM hooks.
- Kubernetes ingress mode.
- Static files if truly needed.

---

# 7. Технический стек

Основной язык:

```txt
Rust
```

Рекомендуемые библиотеки:

```txt
tokio
hyper или pingora
serde
serde_json
serde_derive
clap
tracing
tracing-subscriber
thiserror
anyhow
notify
prometheus или metrics
rustls в будущей версии
```

## Важное архитектурное решение

Если выбран `hyper`, нужно аккуратно реализовать proxy pipeline вручную.

Если выбран `pingora`, нужно использовать его как основу для reverse proxy и не изобретать низкоуровневые механизмы заново.

Предпочтительный подход для production-like реализации:

```txt
Pingora-first, если подходит по API и зрелости.
Hyper-first, если нужен максимальный контроль и образовательная прозрачность.
```

На старте допустимо выбрать `hyper + tokio`, если цель — полностью контролируемая минимальная реализация.

---

# 8. Архитектура проекта

Предлагаемая структура репозитория:

```txt
gatepup/
  Cargo.toml
  README.md
  LICENSE
  Dockerfile
  docker-compose.yml
  config.example.json

  crates/
    gatepup-cli/
      src/
        main.rs

    gatepup-core/
      src/
        lib.rs
        app.rs
        error.rs

    gatepup-config/
      src/
        lib.rs
        model.rs
        loader.rs
        validator.rs

    gatepup-proxy/
      src/
        lib.rs
        server.rs
        router.rs
        upstream.rs
        balancer.rs
        health.rs
        retry.rs
        timeout.rs

    gatepup-observability/
      src/
        lib.rs
        logging.rs
        metrics.rs
        request_id.rs

    gatepup-admin/
      src/
        lib.rs
        api.rs
        health.rs
```

Если на старте хочется проще, можно начать с одного crate:

```txt
src/
  main.rs
  config.rs
  proxy.rs
  router.rs
  upstream.rs
  balancer.rs
  health.rs
  metrics.rs
  error.rs
```

Но архитектурно держать границы модулей так, чтобы позже можно было вынести их в отдельные crates.

---

# 9. Основные компоненты

## 9.1 Config loader

Отвечает за:

- чтение JSON-файла;
- парсинг через `serde`;
- валидацию;
- построение runtime snapshot;
- понятные ошибки.

Команда:

```bash
gatepup validate ./config.json
```

должна проверять конфиг и выводить список ошибок.

## 9.2 Config snapshot

В runtime нельзя использовать хаотичный mutable config.

Нужно использовать immutable snapshot:

```rust
Arc<ConfigSnapshot>
```

В будущем hot reload должен работать так:

```txt
1. прочитать новый config.json
2. распарсить
3. провалидировать
4. построить новый ConfigSnapshot
5. атомарно заменить активный snapshot
6. новые запросы используют новый snapshot
7. старые запросы завершаются на старом snapshot
```

## 9.3 Router

Router должен уметь:

- искать route по host;
- искать route по path prefix;
- выбирать самый специфичный route;
- отдавать upstream name.

Правила приоритета:

```txt
exact host > wildcard host
longest path prefix > shorter path prefix
exact path > prefix path
higher explicit priority > lower priority
```

Для MVP можно начать с:

```txt
exact host + pathPrefix
```

## 9.4 Upstream manager

Отвечает за:

- список upstream groups;
- список targets;
- состояние здоровья targets;
- выбор target через load balancer;
- исключение unhealthy targets;
- fallback, если все targets unhealthy.

## 9.5 Load balancer

MVP:

```txt
round_robin
```

Позже:

```txt
least_connections
weighted_round_robin
random
ip_hash
```

## 9.6 Health checker

Active health checks:

- периодически отправлять HTTP-запрос к target;
- считать target healthy/unhealthy;
- учитывать thresholds.

Passive health checks:

- если backend возвращает connect error / timeout / 5xx, увеличивать failure counter;
- после threshold временно помечать backend как unhealthy.

## 9.7 Proxy server

Должен:

- принять request;
- найти route;
- выбрать upstream target;
- собрать upstream URI;
- проксировать headers/body;
- не читать весь body в память;
- поддерживать streaming;
- корректно возвращать ошибку, если route/upstream не найден.

## 9.8 Observability

Нужны:

- JSON access logs;
- request id;
- latency;
- status code;
- upstream target;
- route name;
- error kind;
- metrics endpoint.

Пример access log:

```json
{
  "ts": "2026-01-01T12:00:00Z",
  "level": "info",
  "request_id": "01H...",
  "method": "GET",
  "host": "api.example.com",
  "path": "/users",
  "route": "api",
  "upstream": "http://api-1:3000",
  "status": 200,
  "duration_ms": 12
}
```

---

# 10. JSON config v0.1

Сделай JSON-конфиг человекочитаемым и пригодным для UI/API.

Пример:

```json
{
  "app": {
    "name": "gatepup",
    "logLevel": "info"
  },
  "listeners": [
    {
      "name": "public-http",
      "bind": "0.0.0.0:80",
      "protocol": "http",
      "routes": [
        {
          "name": "frontend",
          "match": {
            "host": "example.com",
            "pathPrefix": "/"
          },
          "upstream": "frontend"
        },
        {
          "name": "api",
          "match": {
            "host": "api.example.com",
            "pathPrefix": "/"
          },
          "upstream": "api"
        }
      ]
    }
  ],
  "upstreams": [
    {
      "name": "frontend",
      "loadBalancing": "round_robin",
      "targets": [
        {
          "url": "http://frontend:3000",
          "weight": 1
        }
      ],
      "healthCheck": {
        "enabled": true,
        "path": "/",
        "intervalSeconds": 10,
        "timeoutMs": 1000,
        "healthyThreshold": 2,
        "unhealthyThreshold": 3
      }
    },
    {
      "name": "api",
      "loadBalancing": "round_robin",
      "targets": [
        {
          "url": "http://api-1:4000",
          "weight": 1
        },
        {
          "url": "http://api-2:4000",
          "weight": 1
        }
      ],
      "healthCheck": {
        "enabled": true,
        "path": "/health",
        "intervalSeconds": 10,
        "timeoutMs": 1000,
        "healthyThreshold": 2,
        "unhealthyThreshold": 3
      }
    }
  ],
  "admin": {
    "enabled": true,
    "bind": "127.0.0.1:8080"
  },
  "metrics": {
    "enabled": true,
    "path": "/metrics"
  }
}
```

---

# 11. Config model requirements

Создай Rust-модели через `serde`.

Примерно:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct GatePupConfig {
    pub app: AppConfig,
    pub listeners: Vec<ListenerConfig>,
    pub upstreams: Vec<UpstreamConfig>,
    pub admin: Option<AdminConfig>,
    pub metrics: Option<MetricsConfig>,
}
```

Обязательные проверки:

- listener names unique;
- upstream names unique;
- route names unique внутри listener;
- каждый route ссылается на существующий upstream;
- bind address валидный;
- upstream target URL валидный;
- health check timeout меньше interval;
- no empty targets;
- no empty route match;
- no duplicate route conflicts, если можно определить.

Ошибки должны быть понятные:

```txt
Route "api" references unknown upstream "api-service"
```

а не просто:

```txt
Invalid config
```

---

# 12. HTTP behavior

## Если route не найден

Вернуть:

```txt
404 Not Found
```

Body:

```json
{
  "error": "route_not_found"
}
```

## Если upstream не найден

Это ошибка конфигурации, но если произошло в runtime:

```txt
502 Bad Gateway
```

## Если все targets unhealthy

Вернуть:

```txt
503 Service Unavailable
```

Body:

```json
{
  "error": "no_healthy_upstream"
}
```

## Если upstream timeout

Вернуть:

```txt
504 Gateway Timeout
```

Body:

```json
{
  "error": "upstream_timeout"
}
```

## Если upstream connect error

Вернуть:

```txt
502 Bad Gateway
```

Body:

```json
{
  "error": "upstream_connect_error"
}
```

---

# 13. Headers

MVP должен корректно выставлять:

```txt
X-Forwarded-For
X-Forwarded-Host
X-Forwarded-Proto
X-Request-Id
```

Необходимо сохранить большинство исходных headers, но аккуратно обработать hop-by-hop headers.

Удалять или не прокидывать:

```txt
Connection
Keep-Alive
Proxy-Authenticate
Proxy-Authorization
TE
Trailer
Transfer-Encoding
Upgrade
```

Для WebSocket в будущей версии потребуется отдельная логика Upgrade.

---

# 14. Retry policy

В MVP retries можно не делать, но архитектурно подготовить.

Когда retries появятся, нельзя retry-ить все методы подряд.

Безопасные дефолты:

```json
{
  "retries": {
    "enabled": true,
    "attempts": 2,
    "methods": ["GET", "HEAD", "OPTIONS"],
    "retryOn": ["connect_timeout", "upstream_5xx"]
  }
}
```

Не делать retry для:

```txt
POST
PUT
PATCH
DELETE
```

если пользователь явно не разрешил это и не понимает риск.

---

# 15. Reliability requirements

GatePup должен быть failure-aware.

Нужно предусмотреть:

- graceful shutdown;
- graceful config reload в будущей версии;
- no panic in request path;
- no unwrap в production code;
- bounded timeouts;
- bounded buffers;
- health-based routing;
- clear error mapping;
- structured logs for failures;
- metrics for failures.

Нельзя допускать, чтобы один плохой backend ломал весь proxy.

---

# 16. Performance requirements

Принципы:

- async I/O through Tokio;
- streaming request/response bodies;
- avoid reading full body into memory;
- minimal allocations in hot path;
- precompiled route table;
- immutable config snapshot;
- low-lock shared state;
- connection reuse to upstreams;
- no blocking operations in request path;
- health checks in background tasks.

Не оптимизировать преждевременно, но не закладывать архитектуру, которая будет медленной изначально.

---

# 17. Security defaults

По умолчанию:

- admin API слушает только `127.0.0.1`;
- metrics можно отключить;
- detailed internal errors не отдаются клиенту;
- sensitive headers не логируются;
- config validation строгий;
- no remote config write в MVP;
- no unauthenticated public admin API.

В будущем для admin API:

- token auth;
- IP allowlist;
- optional basic auth;
- optional mTLS.

---

# 18. Admin API v0.1

Минимум:

```txt
GET /health
GET /routes
GET /upstreams
GET /config/effective
```

Пример:

```json
{
  "status": "ok",
  "version": "0.1.0"
}
```

`/upstreams` должен показывать health targets:

```json
{
  "upstreams": [
    {
      "name": "api",
      "targets": [
        {
          "url": "http://api-1:4000",
          "healthy": true,
          "lastCheck": "2026-01-01T12:00:00Z"
        }
      ]
    }
  ]
}
```

---

# 19. Metrics

Для MVP достаточно:

```txt
gatepup_requests_total
gatepup_request_duration_seconds
gatepup_upstream_requests_total
gatepup_upstream_errors_total
gatepup_upstream_healthy
gatepup_route_not_found_total
```

Если используется Prometheus format, `/metrics` должен быть совместим с Prometheus scraping.

---

# 20. CLI

CLI должен быть понятный.

```bash
gatepup run --config ./config.json
gatepup validate --config ./config.json
gatepup print-config --config ./config.json
```

Будущие команды:

```bash
gatepup routes
gatepup upstreams
gatepup reload
gatepup doctor
```

---

# 21. Docker

Нужен production-friendly Dockerfile.

Требования:

- multi-stage build;
- маленький final image;
- запуск non-root user;
- config mount через volume;
- порт 80 и 8080;
- healthcheck.

Пример запуска:

```bash
docker run \
  -p 80:80 \
  -p 8080:8080 \
  -v ./config.json:/etc/gatepup/config.json \
  gatepup/gatepup:latest
```

Пример `docker-compose.yml`:

```yaml
services:
  gatepup:
    image: gatepup/gatepup:latest
    ports:
      - "80:80"
      - "8080:8080"
    volumes:
      - ./config.json:/etc/gatepup/config.json
    depends_on:
      - api

  api:
    image: node:22-alpine
    working_dir: /app
    command: ["node", "server.js"]
```

---

# 22. Тестирование

Обязательные тесты:

## Unit tests

- config parsing;
- config validation;
- route matching;
- upstream selection;
- round-robin behavior;
- health status transitions;
- error mapping.

## Integration tests

- proxy request to backend;
- route not found;
- upstream unavailable;
- unhealthy upstream excluded;
- multiple upstreams;
- headers forwarding;
- timeout behavior.

## Load tests later

Можно добавить:

```txt
wrk
oha
bombardier
vegeta
```

Но в первую очередь нужен корректный behavior.

---

# 23. Coding rules

Пиши код так, как будто это production infrastructure component.

Правила:

- no `unwrap()` в runtime path;
- no `expect()` кроме startup/test;
- ошибки через `thiserror`;
- application-level errors понятные;
- modules small and focused;
- public API аккуратный;
- comments only where they explain non-obvious logic;
- use `tracing` instead of `println`;
- all config structs derive `Debug`, `Clone`, `Deserialize`;
- avoid global mutable state;
- use `Arc` thoughtfully;
- avoid blocking calls in async context.

---

# 24. README requirements

README должен содержать:

- что такое GatePup;
- зачем он нужен;
- что он умеет;
- что он НЕ пытается делать;
- быстрый старт;
- пример config.json;
- Docker example;
- CLI commands;
- roadmap;
- development instructions;
- license.

Первый абзац README:

```md
# GatePup

Tiny watchdog for your web traffic.

GatePup is a lightweight, ultra-fast and resilient reverse proxy written in Rust.
It is designed for simple Docker-based deployments, self-hosted apps and small
production environments where traditional reverse proxies may feel too complex.
```

---

# 25. Definition of Done для MVP

MVP считается готовым, если:

- можно запустить GatePup локально;
- можно указать `config.json`;
- proxy принимает HTTP-запрос;
- route выбирается по host/path;
- request проксируется в backend;
- round-robin работает на нескольких targets;
- health checks исключают dead target;
- если backend упал, пользователь получает корректную 502/503/504;
- есть JSON access logs;
- есть `/health`;
- есть `/metrics`;
- есть Docker image;
- есть README;
- есть config.example.json;
- основные unit/integration тесты проходят.

---

# 26. Рекомендуемый порядок разработки

Следуй этому порядку:

## Step 1

Создать Rust project structure.

## Step 2

Реализовать config models.

## Step 3

Реализовать config loader + validator.

## Step 4

Реализовать простой HTTP server.

## Step 5

Реализовать routing.

## Step 6

Реализовать single upstream proxy.

## Step 7

Реализовать upstream groups.

## Step 8

Реализовать round-robin balancing.

## Step 9

Реализовать health checks.

## Step 10

Реализовать structured logs.

## Step 11

Реализовать admin `/health`.

## Step 12

Реализовать `/metrics`.

## Step 13

Добавить Dockerfile.

## Step 14

Добавить integration tests.

## Step 15

Улучшить README и examples.

---

# 27. Первый конкретный запрос к IDE-агенту

Начни разработку GatePup с MVP v0.1.

Сначала создай структуру проекта на Rust.

Затем реализуй:

1. `config.example.json`;
2. Rust-модели конфига через `serde`;
3. загрузку JSON-конфига из файла;
4. валидацию конфига;
5. CLI-команды:
   - `gatepup validate --config ./config.example.json`;
   - `gatepup print-config --config ./config.example.json`.

Пока не реализуй сам proxy-server. Сначала сделай качественную основу config/CLI.

Требования:

- код должен компилироваться;
- должны быть unit tests для config validation;
- ошибки должны быть понятными;
- не использовать `unwrap()` в production code;
- README должен содержать краткий quick start;
- использовать `tracing` для логов;
- использовать `clap` для CLI;
- использовать `thiserror` для ошибок.

После завершения первого шага предложи следующий commit/этап: HTTP server + route matching.

---

# 28. Дополнительная идея на будущее

В будущем GatePup может получить UI, где пользователь сможет визуально управлять routes/upstreams.

Но core должен оставаться независимым:

```txt
config file/API → config validation → config snapshot → proxy runtime
```

UI не должен быть обязательным для работы proxy.

---

# 29. Общий стиль продукта

GatePup должен ощущаться как:

```txt
small but serious
cute but production-aware
simple but not toy
developer-friendly
Docker-first
safe by default
```

Главная инженерная философия:

```txt
Do less, but do it extremely well.
```
