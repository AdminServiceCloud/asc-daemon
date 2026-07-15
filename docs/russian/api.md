# 📡 API демона: gRPC (ConnectRPC) + REST

## 📌 Описание

API-сервер демона: два транспорта на **одном порту** — gRPC (tonic; совместим с ConnectRPC-клиентами платформы) и REST (JSON поверх HTTP). Оба вызывают один сервисный слой и защищены одной токен-аутентификацией. Контракты — protobuf-файлы в [proto/](../../proto/asc/daemon/v1/daemon.proto), они же источник правды для клиентов платформы (buf-зависимость).

## 🎯 Сценарии использования

- Платформа AdminService.Cloud управляет нодой через gRPC/ConnectRPC (через туннель nodeservice).
- Скрипты и сторонние интеграции используют REST: `curl -H "Authorization: Bearer <token>" http://127.0.0.1:8420/v1/apps`.
- Перед открытием WebSocket-консоли клиент запрашивает **временный консольный токен** (`IssueConsoleToken`) — платформа делает это автоматически после проверки прав.

## 🏗️ Техническое решение

### Сервер

- Слушает `127.0.0.1:8420` (настройка `[api] listen` в config.toml). Наружу порт не открывается — удалённый доступ идёт через туннель платформы (DMN-005 → tunnel).
- Один listener: axum-роутер REST + tonic-маршруты gRPC (h2c), диспетчеризация по пути/Content-Type.
- Все блокирующие операции (docker/systemctl/git) уходят в `spawn_blocking` — event loop не блокируется.

### 🔐 Аутентификация

- Bearer-токен: генерируется демоном при первом старте (32 байта из CSPRNG), хранится в `config.toml` (файл root-only, 0600).
- Обязателен для обоих транспортов: REST — заголовок `Authorization: Bearer <token>`, gRPC — metadata `authorization`. Сравнение — constant-time.
- Без токена: REST → `401 {"error": ...}`, gRPC → `UNAUTHENTICATED`.
- Пер-пользовательские API-токены (видимость группы приложений по владельцу, как в CLI) — задача после MVP; сейчас API-вызовы действуют с полной видимостью, права пользователей проверяет платформа.

### 🎫 Временные консольные токены

- `AppService.IssueConsoleToken(app_id, session)` / `POST /v1/apps/{id}/console-token {"session": "logs"|"attach"}`.
- Токен одноразовый, TTL 30 секунд, привязан к приложению и типу сессии; хранится в памяти демона.
- WebSocket-консоль (DMN-007) принимает подключение только по такому токену.

### 🗺️ Маршруты REST ↔ методы gRPC

| REST | gRPC | Описание |
|---|---|---|
| `GET /v1/status` | `DaemonService.GetStatus` | Версия, счётчики приложений |
| `GET /v1/apps` | `AppService.ListApps` | Список приложений |
| `POST /v1/apps {"spec": "name@ver"}` | `AppService.InstallApp` | Установка из реестра |
| `GET /v1/apps/{id}` | `AppService.GetApp` | Одно приложение |
| `GET /v1/apps/{id}/disk` | `AppService.GetAppDisk` | Дисковое пространство: образ, репозиторий, данные, кастомные тома |
| `POST /v1/apps/{id}/start\|stop\|restart` | `AppService.Start/Stop/RestartApp` | Жизненный цикл |
| `GET /v1/apps/{id}/logs?tail=N` | `AppService.GetAppLogs` | Хвост логов |
| `DELETE /v1/apps/{id}` | `AppService.RemoveApp` | Удаление с данными |
| `POST /v1/apps/{id}/console-token` | `AppService.IssueConsoleToken` | Временный токен консоли |
| `GET /v1/metrics` | `MonitorService.GetSystemMetrics` | Текущие системные метрики (503, пока нет первого сэмпла) |
| `GET /v1/metrics/history?limit=N` | `MonitorService.GetMetricsHistory` | История метрик из кольцевого буфера, старые → новые |

### 📜 Кодогенерация

- Rust-код генерируется из `proto/` в build.rs через **protox** (pure-Rust компилятор protobuf) + tonic-build — системный `protoc` не нужен, сборка герметична.
- Изменения контрактов — только обратно совместимые (новые поля — optional, номера не переиспользовать).

## 🔗 Связанные задачи

DMN-005, DMN-007 в [ROADMAP.md](../../../asc-platform/ROADMAP.md).
