# 📦 Пакетный менеджер и реестры

## 📌 Описание

Пакетный менеджер в духе apt/homebrew: приложения описываются манифестом `asc.yaml`, публикуются в реестрах, ставятся командой `asc install <package>`. Реестры бывают официальные и кастомные, локальные (`file://`) и удалённые (`https://`), включая GitHub-репозитории.

## 🎯 Сценарии использования

- `asc install nginx` — установка из официального реестра.
- `asc source add https://registry.example.com` — подключение кастомного реестра (как apt source).
- `asc source add https://github.com/user/my-app` — приложение прямо из GitHub (asc.yaml в корне); для приватного при 404 — предложение подключить токен.
- Магазин платформы ([🛍️ app-store](../../asc-platform/docs/features/app-store.md)) — витрина над теми же реестрами.

## 🏗️ Техническое решение

### Манифест asc.yaml (черновик)

```yaml
name: my-app
version: 1.2.0
type: docker | native | utility
category: web                 # тема для реестра (databases, ai, bots, game-servers…)
description: "..."            # EN в официальном реестре
settings: ./asc.settings.yaml # необязательно: файл настроек приложения (см. ниже)
runtime:
  image: nginx:1.27           # для docker
  # или install/start/stop команды для native
env:
  - name: PORT
    default: 8080
ports: [8080]
volumes: [/data]
requirements: { ram: 256M, disk: 1G }
healthcheck: { http: /health }
```

> 📐 JSON-схемы манифестов: [asc.schema.json](../../registry/schema/asc.schema.json), [asc.stack.schema.json](../../registry/schema/asc.stack.schema.json) и [asc.settings.schema.json](../../registry/schema/asc.settings.schema.json) в репозитории `registry`.

### Настройки приложения: asc.settings.yaml

Настройки приложения выносятся в отдельный файл, на который ссылается `asc.yaml` (`settings: ./asc.settings.yaml`). В нём описываются параметры: тип, лимиты, перечисления, значения по умолчанию — при установке пользователь заполняет их (UI платформы рисует форму, CLI задаёт вопросы; в silent-режиме берутся дефолты):

```yaml
settings:
  - key: server_name
    type: string                # string | number | boolean | enum | secret
    title: "Server name"
    default: "My Server"
    required: true

  - key: max_players
    type: number
    default: 10
    limits: { min: 1, max: 200 }   # лимиты значений

  - key: difficulty
    type: enum                  # перечисление
    values: [peaceful, easy, normal, hard]
    default: normal

  - key: rcon_password
    type: secret                # хранится как секрет, маскируется
    required: true

  - key: enable_backups
    type: boolean
    default: true
```

- Значения настроек сохраняются в `/asc/apps/<id>/config/` и пробрасываются приложению (env/конфиг-файл — по шаблону из манифеста).
- Изменение настроек — `asc app config <name>` или UI платформы; после изменения — перезапуск приложения.

### 🚀 Команда запуска (start_command)

Команда запуска приложения настраивается в `asc.settings.yaml` (переопределяет `runtime.start` из `asc.yaml`). В строку можно **подставлять environment-переменные** приложения — синтаксис `${VAR}`:

```yaml
start_command: "java -Xmx${MEM_LIMIT}M -jar server.jar --port ${PORT} --level ${LOG_LEVEL}"
```

- Подстановка выполняется демоном при запуске из итогового окружения приложения (настройки + env-уровни организации/ноды/приложения, см. [🌱 environments](../../asc-platform/docs/features/environments.md)).
- Неразрешённая переменная — ошибка запуска с указанием имени переменной.
- UI платформы показывает вычисленный предпросмотр команды (секреты маскируются).
- Изменение команды — как и настроек: применяется после перезапуска приложения.

### 🐳 Скрипты install/update: native или docker

Скрипты `install` и `update` пакета могут исполняться **как нативно на хосте, так и в docker** — управляется полем `run_in` в блоке `scripts:` манифеста:

```yaml
scripts:
  install:
    run: ./scripts/install.sh
    run_in: native            # native | docker
  update:
    run: ./scripts/update.sh
    run_in: docker            # одноразовый контейнер
    image: debian:12          # образ для docker-исполнения (опционально)
```

- `native` — скрипт исполняется на хосте от пользователя приложения.
- `docker` — скрипт исполняется в одноразовом контейнере с примонтированным каталогом `/asc/apps/<id>/` (изоляция зависимостей сборки от хоста).
- По умолчанию `run_in` наследует `type` пакета: `docker` → docker, `native`/`utility` → native.

### Механика установки: клонирование репозитория

Установка приложения = **клонирование его репозитория**:

1. `asc install <package>` → демон клонирует репозиторий пакета в `/asc/apps/<id>/repository/`.
2. **Версии приложения = git-теги** (GitHub tags): установка конкретной версии — `asc install <package>@1.2.0` (checkout тега), обновление — `asc app upgrade <name>` (checkout нового тега).
3. Дальше демон работает с локальной копией: читает `asc.yaml`/`asc.settings.yaml`, собирает/запускает по типу приложения.

**Приватные репозитории**: чтобы CLI мог корректно склонировать закрытый репозиторий, для источника настраивается доступ — `asc source add <url> --token <token>` (или деплой-ключ); токен хранится в защищённом хранилище демона и используется при clone/fetch. При 404 от GitHub демон предлагает подключить токен.

### Несколько приложений в одном репозитории: asc.stack.yaml

**Правило корня**: репозиторий-пакет может содержать сколько угодно `asc.yaml` в подкаталогах, но в его корне обязан лежать **ровно один** манифест — либо `asc.yaml` (одиночное приложение), либо `asc.stack.yaml` (стек), который соединяет все вложенные `asc.yaml`. Вложенные манифесты без корневого не индексируются.

Манифест-стек `asc.stack.yaml` перечисляет приложения и пути к их `asc.yaml`:

```yaml
name: my-stack
version: 1.0.0
description: "..."
apps:
  - name: web
    path: ./web            # каталог с asc.yaml
  - name: worker
    path: ./worker
  - name: db
    path: ./db
    optional: true          # необязательный компонент стека
```

- `asc install my-stack` — установка всего стека; `asc install my-stack/web` — только одного приложения из него.
- Стек может объявлять общие `env` и зависимости между приложениями (`depends_on` — порядок запуска), компоненты могут быть `optional`.
- Реестры и магазин платформы индексируют как одиночные `asc.yaml`, так и стеки `asc.stack.yaml`.
- Примеры — в репозитории [asc-example-apps](../../asc-example-apps).

### Реестры

- **Формат реестра** (repo `registry`) — иерархия JSON-файлов: корневой индекс `registry.json` → файлы категорий `categories/<тема>.json` (databases, ai, bots, game-servers, system-utilities, web…) → опциональные подкатегории (`children`). Пакеты бывают двух типов: `app` (asc.yaml) и `stack` (asc.stack.yaml). Схемы валидации — в `registry/schema/`. Описания — на английском.
- **Дерево источников**: sourcelist демона → из всех реестров строится дерево (по ссылкам `index`/`children` корневого индекса), затем объединённый список приложений (для каждого пользователя); конфликт имён решает приоритет источника.
- **Типы источников**: `file://` (локальный каталог) и `https://` (реестр, GitHub raw).
- **Source list по пользователям**: два уровня списков —
  - **системный** `/etc/asc/sources.toml` — правит root (`sudo asc source add|remove`), источники видны **всем** пользователям сервера;
  - **пользовательский** `~/.config/asc/sources.toml` — каждый пользователь ведёт свой список (`asc source add|remove` без sudo), он дополняет системный.
  - Эффективный список = системные источники (приоритетнее) + свои; затенять и удалять системные источники пользователь не может (`asc source list` показывает происхождение каждого источника). Кэш индексов у root — в `data_dir`, у пользователя — в `~/.cache/asc/`.
- **Политика установки** (`[policy]` в `/etc/asc/config.toml`, правит root): `user_install = "all"` (по умолчанию — пользователи ставят любые пакеты: Docker, нативные, утилиты) или `user_install = "docker"` (пользователям — только Docker-приложения; нативные и утилиты ставит только root). Применяется при `asc install`; на root не действует.
- **Кэш индексов** с TTL + `asc update` для принудительного обновления.
- **CLI**: `asc install|remove|upgrade <pkg>`, `asc search <query>`, `asc source add|remove|list`, `asc update`.

## 🔗 Связанные задачи

DMN-003, DMN-018, REG-001, REG-002, BE-002, BE-003 в [ROADMAP.md](../../asc-platform/ROADMAP.md); GRW-011 в [ROADMAP-GROWTH.md](../../asc-platform/ROADMAP-GROWTH.md).
