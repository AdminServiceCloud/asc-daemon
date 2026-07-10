# 🦀 asc-daemon — AdminService.Cloud Daemon

> 🌍 **Язык:** Русский · [🇬🇧 English version](../../README.md)

**Open source утилита для управления runtime-приложениями в Linux**: демон и CLI на Rust, которые запускают и обслуживают приложения любого рантайма — Docker-контейнеры, нативные приложения (systemd) и обычные процессы. В комплекте: пакетный менеджер, бекапы, мониторинг, MCP-сервер для AI. Работает автономно через CLI или как агент SaaS-платформы [AdminService.Cloud](https://adminservice.cloud).

## ✨ Возможности

- 📱 **Управление приложениями** — Docker-контейнеры, нативные приложения (systemd) и системные утилиты
- 💻 **CLI** — полное управление сервером из терминала (`asc ...`)
- 📡 **ConnectRPC + REST API** — proto-контракты API живут в этом репозитории (`proto/`) и открыты вместе с демоном; платформа AdminService.Cloud линкует их отсюда. REST (JSON/HTTP) работает одновременно с ConnectRPC на одном сервере — из тех же контрактов
- 📦 **Пакетный менеджер** — манифест `asc.yaml`, реестры (как в apt) и установка через `asc install <package>`
- 🤖 **MCP-сервер** — управление сервером через AI (Claude Code, Claude Desktop, платформа ASC)
- 💾 **Бекапы** — полные и инкрементальные, локальные и в облако (S3/SFTP), с ротацией
- 📊 **Мониторинг** — метрики системы и приложений, healthcheck'и
- 📁 **SFTP-сервер** — файловый доступ с изоляцией по конкретному приложению
- 🗄️ **Базы данных** — создание баз и пользователей (PostgreSQL, MySQL, MongoDB, Redis)
- 🖥️ **Консоли** — WebSocket-терминал приложений и SSH-консоль для UI
- ⏰ **Scheduler** — задачи по расписанию (cron), очередь с приоритетами
- 🔄 **asc-updater** — отдельная утилита обновлений: автообновления (отключаемые), каналы stable/beta, откат; при установке — выбор настроек по умолчанию или своих
- 🧠 **Skills для AI-агентов** — готовые навыки для Claude Code и других нейронок в каталоге [skills/](../../skills/README.md)

## ⚡ Установка

```bash
# интерактивно: покажет настройки по умолчанию и спросит — принять или изменить
curl -fsSL https://raw.githubusercontent.com/AdminServiceCloud/asc-daemon/main/install.sh | sudo bash

# silent-режим: одна команда, всё ставится с настройками по умолчанию без вопросов
curl -fsSL https://raw.githubusercontent.com/AdminServiceCloud/asc-daemon/main/install.sh | sudo bash -s -- --silent
```

Настроить можно и после silent-установки: `asc-updater` + `/etc/asc/config.toml`.

## ⌨️ Быстрый старт

```bash
asc service install        # ⚙️ установить API-сервис демона как systemd-юнит (автозапуск)
asc service start|status   # 🚀 запустить сервис / проверить состояние
asc status                 # 📊 состояние сервера и приложений
asc install helloworld     # 📦 установка приложения из реестра
asc app logs helloworld    # 📜 логи приложения
asc config lang ru         # 🌍 сменить язык вывода команд (en|ru)
asc connect <token>        # ☁️ подключение к платформе AdminService.Cloud
asc mcp serve              # 🤖 запуск MCP-сервера для AI-клиентов
```

## 🧠 Skills для Claude Code и других нейронок

В каталоге [skills/](../../skills/README.md) — готовые навыки (Agent Skills), которые учат AI-агентов управлять сервером через `asc`:

```bash
# Claude Code: подключить скиллы себе (все проекты) или в проект
cp -r skills/* ~/.claude/skills/       # глобально
cp -r skills/* .claude/skills/         # только текущий проект
```

| Скилл | Что умеет |
|---|---|
| [🖥️ asc-server-management](../../skills/asc-server-management/SKILL.md) | Управление сервером: приложения, логи, бекапы, БД. Если `asc` не установлен — проверит, предложит установить из официального репозитория одной командой (silent-режим) |
| [📦 asc-app-packaging](../../skills/asc-app-packaging/SKILL.md) | Упаковка приложений: `asc.yaml` / `asc.stack.yaml`, валидация по схемам, публикация в реестр |

Для MCP-клиентов (Claude Desktop и др.) вместо скиллов — [MCP-сервер демона](../mcp-server.md): `asc mcp serve`.

## 📋 Требования

- 🐧 **ОС**: сейчас поддерживаются **Debian и Ubuntu**; в перспективе — остальные дистрибутивы (CentOS/RHEL, Fedora, Arch и др.) и macOS
- 🧬 **Архитектуры**: x86_64, ARM64, ARMv7
- 🔑 Root/sudo для установки; Docker ставится автоматически при необходимости
- ⚙️ systemd (для `asc service` и автозапуска)

## 📚 Документация

Документация модулей демона — в каталоге [docs/](../README.md):

| Док | Описание |
|---|---|
| [🦀 Обзор демона](../README.md) | Архитектура, API, установка |
| [📡 api](../api.md) | gRPC (ConnectRPC) + REST на одном порту, токены |
| [📱 app-management](../app-management.md) | Docker и нативные приложения, CLI |
| [📦 package-manager](../package-manager.md) | asc.yaml, реестры, `asc install` |
| [🤖 mcp-server](../mcp-server.md) | MCP-сервер для AI |
| [📊 monitoring](../monitoring.md) | Метрики системы и приложений |
| [💾 backups](../backups.md) | Бекапы приложений |
| [📁 sftp](../sftp.md) | SFTP с изоляцией по приложению |
| [🗄️ database](../database.md) | Управление базами данных |
| [🖥️ console](../console.md) | WebSocket- и SSH-консоли |
| [⏰ scheduler](../scheduler.md) | Планировщик задач |
| [🔄 updater](../updater.md) | Утилита asc-updater: автообновления, каналы, откат |

## 🗺️ Roadmap и регламент

Roadmap всего проекта ведётся в репозитории **asc-platform**:

- [🎯 ROADMAP](../../../asc-platform/ROADMAP.md) — задачи демона имеют префикс `DMN-*`
- [🤝 Регламент разработки](../../../asc-platform/AGENTS.md)

> ⚠️ Каталог `old/` — прошлые наработки, используется как справка.

## 🤝 Контрибьют

Правила участия — в [CONTRIBUTING.md](CONTRIBUTING.md) ([English](../../CONTRIBUTING.md)); CI и релизы — GitHub Actions (`.github/workflows/`).

## 📄 Лицензия

[MIT](../../LICENSE) — можно свободно распространять, модифицировать и использовать в коммерческих целях, но с **обязательным сохранением авторства**: Omar El Sayed ([@statebyte](https://github.com/statebyte)), проект AdminService.Cloud, [Anytecture Software](https://anytecture.com).
