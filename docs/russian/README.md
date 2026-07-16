# 🦀 asc-daemon — AdminService.Cloud Daemon

> 🌍 **Язык:** Русский · [🇬🇧 English version](../../README.md)

[![CI](https://github.com/AdminServiceCloud/asc-daemon/actions/workflows/ci.yml/badge.svg)](https://github.com/AdminServiceCloud/asc-daemon/actions/workflows/ci.yml)
[![Release](https://github.com/AdminServiceCloud/asc-daemon/actions/workflows/release.yml/badge.svg)](https://github.com/AdminServiceCloud/asc-daemon/actions/workflows/release.yml)
[![Version](https://img.shields.io/badge/version-0.1.4-blue)](../../version.txt)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](../../LICENSE)
[![Discord](https://img.shields.io/badge/Discord-Join%20us-5865F2?logo=discord&logoColor=white)](https://discord.gg/xzJfp3ePfV)

## 📌 О проекте

**Open source утилита для управления runtime-приложениями в Linux**: демон и CLI на Rust, которые запускают и обслуживают приложения любого рантайма — Docker-контейнеры, нативные приложения (systemd) и обычные процессы. В комплекте: пакетный менеджер, бекапы, мониторинг, MCP-сервер для AI. Работает автономно через CLI или как агент SaaS-платформы [AdminService.Cloud](https://adminservice.cloud). Запускается в любом Linux-окружении, включая **WSL** (Windows Subsystem for Linux).

### ✨ Возможности

- 📱 **Управление приложениями** — Docker-контейнеры, нативные приложения (systemd) и системные утилиты
- 💻 **CLI** — полное управление сервером из терминала (`asc ...`)
- 🧬 **Клонирование инстансов** — `asc app clone <id>`: полная копия инстанса приложения (данные, env, настройки) с новым id; через UI AdminService.Cloud клон можно перенести на другую ноду
- 📡 **ConnectRPC + REST API** — proto-контракты API живут в этом репозитории (`proto/`) и открыты вместе с демоном; платформа AdminService.Cloud линкует их отсюда. REST (JSON/HTTP) работает одновременно с ConnectRPC на одном сервере — из тех же контрактов
- 📦 **Пакетный менеджер** — манифест `asc.yaml`, реестры (как в apt) и установка через `asc install <package>`
- 🤖 **MCP-сервер** — управление сервером через AI (Claude Code, Claude Desktop, платформа ASC)
- 💾 **Бекапы** — `asc backup create|restore|list|prune`, локальное хранилище из коробки, S3/FTP/SFTP настраиваются (перенос данных для них ещё не подключён), исключения через `asc.backup.yaml`, ротация
- 📊 **Мониторинг** — метрики системы и приложений, healthcheck'и
- 📁 **SFTP-сервер** — файловый доступ с изоляцией по конкретному приложению
- 🖥️ **Консоли** — WebSocket-терминал приложений и SSH-консоль для UI
- ⏰ **Scheduler** — задачи по расписанию (cron), очередь с приоритетами
- 🔄 **asc-updater** — отдельная утилита обновлений: автообновления (отключаемые), каналы stable/beta, откат; при установке — выбор настроек по умолчанию или своих
- 🧠 **Skills для AI-агентов** — готовые навыки для Claude Code и других нейронок в каталоге [skills/](../../skills/README.md)

### 💡 Мотивация

Существующие панели решают только часть задачи: Portainer управляет Docker, Pterodactyl — игровыми серверами, классические хостинг-панели — сайтами. Как только на реальном сервере контейнеры соседствуют с нативными сервисами — снова голый SSH. **asc-daemon** создан, чтобы управлять *любым* рантаймом одним инструментом — Docker-контейнерами, systemd-сервисами и обычными процессами — одними и теми же командами, одним API и одним пакетным менеджером. А поскольку серверами всё чаще управляют AI-агенты, демон нативно говорит на MCP — нейронка работает с вашим сервером как полноценный клиент. Демон полностью автономен: всё работает локально через CLI, аккаунт платформы не нужен.

## ⚡ Установка

```bash
curl -fsSL https://raw.githubusercontent.com/AdminServiceCloud/asc-daemon/main/install.sh | sudo bash
```
> интерактивно: покажет настройки по умолчанию и спросит — принять или изменить

```bash
curl -fsSL https://raw.githubusercontent.com/AdminServiceCloud/asc-daemon/main/install.sh | sudo bash -s -- --silent
```
> silent-режим: одна команда, всё ставится с настройками по умолчанию без вопросов

Настроить можно и после silent-установки: `asc-updater` + `/etc/asc/config.toml`.

### 📋 Требования

- 🐧 **ОС**: сейчас поддерживаются **Debian и Ubuntu**, в том числе под **WSL** (Windows Subsystem for Linux); в перспективе — остальные дистрибутивы (CentOS/RHEL, Fedora, Arch и др.) и macOS
- 🧬 **Архитектуры**: x86_64, ARM64, ARMv7
- 🔑 Root/sudo для установки; Docker ставится автоматически при необходимости
- ⚙️ systemd (для `asc service` и автозапуска)

## ⌨️ Быстрый старт

```bash
asc service install
```
> ⚙️ установить API-сервис демона как systemd-юнит (автозапуск)

```bash
asc service start|status
```
> 🚀 запустить сервис / проверить состояние

```bash
asc status
```
> 📊 состояние сервера и приложений

```bash
asc install helloworld
```
> 📦 установка приложения из реестра

```bash
asc app logs helloworld
```
> 📜 логи приложения

```bash
asc app clone helloworld
```
> 🧬 клонировать инстанс приложения (данные, env, настройки)

```bash
asc app settings helloworld
```
> 🎛️ интерактивный редактор настроек (типы, лимиты и enum из asc.settings.yaml)

```bash
asc backup create helloworld
```
> 💾 бекап приложения (по умолчанию — локальное хранилище; восстановление — `asc backup restore <app> <backup-id>`)

```bash
asc config lang ru
```
> 🌍 сменить язык вывода команд (en|ru)

```bash
asc connect <token>
```
> ☁️ подключение к платформе AdminService.Cloud

```bash
asc mcp serve
```
> 🤖 запуск MCP-сервера для AI-клиентов

## 🧠 Skills для Claude Code и других нейронок

В каталоге [skills/](../../skills/README.md) — готовые навыки (Agent Skills), которые учат AI-агентов управлять сервером через `asc`:

```bash
cp -r skills/* ~/.claude/skills/
```
> Claude Code: подключить скиллы себе глобально (все проекты)

```bash
cp -r skills/* .claude/skills/
```
> Claude Code: подключить скиллы только в текущий проект

| Скилл | Что умеет |
|---|---|
| [🖥️ asc-server-management](../../skills/asc-server-management/SKILL.md) | Управление сервером: приложения, логи, бекапы. Если `asc` не установлен — проверит, предложит установить из официального репозитория одной командой (silent-режим) |
| [📦 asc-app-packaging](../../skills/asc-app-packaging/SKILL.md) | Упаковка приложений: `asc.yaml` / `asc.stack.yaml`, валидация по схемам, публикация в реестр |

Для MCP-клиентов (Claude Desktop и др.) вместо скиллов — [MCP-сервер демона](mcp-server.md): `asc mcp serve`.

## 📚 Документация

Документация модулей демона — в каталоге docs/russian/ (🇬🇧 [English version](../english/README.md)):

| Док | Описание |
|---|---|
| [🦀 Обзор демона](overview.md) | Архитектура, API, установка |
| [📡 api](api.md) | gRPC (ConnectRPC) + REST на одном порту, токены |
| [📱 app-management](app-management.md) | Docker и нативные приложения, CLI |
| [📦 package-manager](package-manager.md) | asc.yaml, реестры, `asc install` |
| [🤖 mcp-server](mcp-server.md) | MCP-сервер для AI |
| [📊 monitoring](monitoring.md) | Метрики системы и приложений |
| [💾 backups](backups.md) | Бекапы приложений |
| [📁 sftp](sftp.md) | SFTP с изоляцией по приложению |
| [🖥️ console](console.md) | WebSocket- и SSH-консоли |
| [⏰ scheduler](scheduler.md) | Планировщик задач |
| [🔄 updater](updater.md) | Утилита asc-updater: автообновления, каналы, откат |

## 🗺️ Roadmap

Roadmap всего проекта ведётся в репозитории **asc-platform**:

- [🎯 ROADMAP](../../../asc-platform/ROADMAP.md) — задачи демона имеют префикс `DMN-*`
- [🤝 Регламент разработки](../../../asc-platform/AGENTS.md)

> ⚠️ Каталог `old/` — прошлые наработки, используется как справка.

## 💬 Поддержка

Связаться с мейнтейнерами можно любым из способов:

- 🐛 [GitHub Issues](https://github.com/AdminServiceCloud/asc-daemon/issues) — баги и запросы фич (есть шаблоны)
- ❓ [GitHub Discussions](https://github.com/AdminServiceCloud/asc-daemon/discussions) — вопросы и идеи
- 💬 [Discord](https://discord.gg/xzJfp3ePfV) — официальный сервер сообщества: общение, помощь, анонсы
- ☁️ [adminservice.cloud](https://adminservice.cloud) — сайт платформы и контакты

## 🌟 Помощь проекту

Хотите сказать **спасибо** или поддержать активную разработку asc-daemon:

- ⭐ Поставьте звезду репозиторию на GitHub
- 🐦 Расскажите о проекте в соцсетях
- 📝 Напишите о проекте в блоге или на митапе
- 💬 Заходите в [Discord-сообщество](https://discord.gg/xzJfp3ePfV)
- 🤝 [Контрибьютьте](../../CONTRIBUTING.md) — код, доки, переводы, пакеты для реестра

## 🌠 История звёзд

[![Star History Chart](https://api.star-history.com/svg?repos=AdminServiceCloud/asc-daemon&type=Date)](https://star-history.com/#AdminServiceCloud/asc-daemon&Date)

## 🤝 Контрибьют

Правила участия — в [CONTRIBUTING.md](CONTRIBUTING.md) ([English](../../CONTRIBUTING.md)); CI и релизы — GitHub Actions (`.github/workflows/`). Разрабатываете на Windows? Используйте **WSL** (Ubuntu) для сборки и тестов проекта: `cargo build` / `cargo test` выполняются в WSL, а `cargo check` / `clippy` работают с хоста под Linux-таргет (см. `.cargo/config.toml`). Каждый pull request автоматически получает ревью от владельца кода ([@statebyte](https://github.com/statebyte)) через [CODEOWNERS](../../.github/CODEOWNERS).

Перед участием прочитайте наш [🤝 Кодекс поведения](CODE_OF_CONDUCT.md) — мы за доброжелательное сообщество без харассмента.

## 👥 Авторы и контрибьюторы

Репозиторий создан **Omar El Sayed** ([@statebyte](https://github.com/statebyte)), AdminService.Cloud, [Anytecture Software](https://anytecture.com).

Полный список авторов и контрибьюторов — на [странице контрибьюторов](https://github.com/AdminServiceCloud/asc-daemon/graphs/contributors).

## 🛡️ Безопасность

asc-daemon следует хорошим практикам безопасности, но 100% защиту гарантировать нельзя. Программное обеспечение предоставляется **«как есть»**, без каких-либо гарантий.

Нашли уязвимость? Пожалуйста, сообщите о ней приватно — см. [🛡️ Security Policy](SECURITY.md).

## 📄 Лицензия

[MIT](../../LICENSE) — можно свободно распространять, модифицировать и использовать в коммерческих целях, но с **обязательным сохранением авторства**: Omar El Sayed ([@statebyte](https://github.com/statebyte)), проект AdminService.Cloud, [Anytecture Software](https://anytecture.com).
