# 🦀 asc-daemon — обзор

> 🌍 **Язык:** Русский · [🇬🇧 English version](../english/README.md)

![asc-daemon preview](../screenshots/preview.png)

## 📌 Описание

Open source CLI-утилита и демон на Rust, устанавливаемый на сервер пользователя. Работает автономно (полноценный инструмент через CLI) и как агент платформы AdminService.Cloud. Репозиторий: `asc-daemon`.

## ✨ Возможности

| Модуль | Док |
|---|---|
| 📡 API: gRPC (ConnectRPC) + REST, токены | [api](api.md) |
| 📱 Управление приложениями (Docker + нативные) и CLI | [app-management](app-management.md) |
| 📦 Пакетный менеджер (`asc.yaml`, реестры, `asc install`) | [package-manager](package-manager.md) |
| 🤖 MCP-сервер для AI | [mcp-server](mcp-server.md) |
| 📊 Мониторинг системы и приложений | [monitoring](monitoring.md) |
| 💾 Бекапы приложений | [backups](backups.md) |
| 📁 SFTP-сервер по приложению | [sftp](sftp.md) |
| 🖥️ WebSocket- и SSH-консоли | [console](console.md) |
| ⏰ Планировщик задач | [scheduler](scheduler.md) |
| 🔄 Утилита обновлений asc-updater | [updater](updater.md) |

Community-файлы: [🛡️ SECURITY.md](SECURITY.md) — политика безопасности и приватные репорты уязвимостей; [🤝 CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) — кодекс поведения (Contributor Covenant 2.1); [version.txt](../../version.txt) — текущая версия (синхронизируется с `Cargo.toml`); [CODEOWNERS](../../.github/CODEOWNERS) — авто-ревью PR от [@statebyte](https://github.com/statebyte).

## 🏗️ Архитектура

```
🦀 asc-daemon
├── proto/            # 📜 proto-контракты API демона — источник правды (линкуются платформой)
├── src/              # 🦀 все исходники демона
│   ├── cli/          # asc <команды> — общается с демоном через локальный сокет
│   ├── daemon/       # systemd-сервис
│   │   ├── api/      # API: ConnectRPC (proto/) + REST-транспорт из тех же контрактов
│   │   ├── tunnel/   # исходящее соединение к nodeservice (работа за NAT)
│   │   ├── apps/     # драйверы: docker, systemd, process
│   │   ├── pkg/      # пакетный менеджер + реестры
│   │   ├── mcp/      # MCP-сервер
│   │   ├── backup/ monitor/ sftp/ console/ scheduler/
│   │   ├── i18n/     # система переводов вывода команд (EN/RU)
│   │   └── config/   # /etc/asc/config.toml
│   └── updater/      # 🔄 asc-updater — отдельный бинарник обновлений (см. updater.md)
├── skills/           # 🧠 Agent Skills для Claude Code и других нейронок (SKILL.md)
├── docs/             # 📚 документация модулей (english/ + russian/)
├── .github/          # ⚙️ workflows (CI, Release), шаблоны issue/PR
├── CONTRIBUTING.md   # 🤝 правила контрибьюта
├── LICENSE           # 📄 MIT с обязательным авторством
├── Taskfile.yml      # 🛠️ команды разработки, сборки, кросс-компиляции и релиза
└── install.sh        # установка одной командой (ставит asc-updater, тот ставит демон)
```

- **Proto-контракты**: API демона описан protobuf-контрактами **в этом репозитории** (каталог `proto/`) — демон open source, и его контракты публичны вместе с ним. Платформа AdminService.Cloud **линкует контракты отсюда** (buf-зависимость) и генерирует свои клиенты из них; Rust-код демона генерируется из тех же `.proto`-файлов (prost/tonic). Один источник правды — всегда совместимые схемы.
- **REST-транспорт**: помимо ConnectRPC демон отдаёт **REST API** (JSON поверх HTTP: `GET/POST/DELETE /v1/...`) — оба транспорта работают **одновременно на одном HTTP-сервере** и вызывают один и тот же сервисный слой. REST-маршруты выводятся из тех же proto-контрактов (маппинг в стиле `google.api.http`-аннотаций), поэтому схемы не расходятся; аутентификация и правила видимости общие для обоих транспортов (DMN-005).
- **Платформы**: первоочередная поддержка — **Debian и Ubuntu**; архитектура закладывается под остальные дистрибутивы (CentOS/RHEL, Fedora, Arch и др.) и macOS в перспективе — всё дистрибутив-специфичное прячется за абстракциями. Архитектуры: x86_64, ARM64, ARMv7.
- **Управление сервисом**: API-сервис демона запускается через systemd командами самого демона — `asc service install|start|stop|restart|status` (install создаёт systemd-юнит и включает автозапуск).
- **Автономность**: демон полностью работает без платформы (CLI + локальный API) — это принципиально для open source ценности.
- **Подключение к платформе**: `asc connect <token>` — исходящее соединение к nodeservice, mTLS после регистрации.
- **Локализация**: настройка языка хранится в конфиге (`language` в `/etc/asc/config.toml`), выбирается при установке и меняется командой `asc config lang en|ru` — влияет на вывод всех команд через систему переводов (`src/daemon/i18n/`); debug-сообщения не переводятся.
- **Отладочные логи**: `asc config debug on|off` переключает `[log] level` между `debug` и `info` в `config.toml` (`RUST_LOG` по-прежнему в приоритете); трассировка теперь инициализируется в любой команде, а не только в `asc serve` — например, `asc install` печатает в stderr прогресс скачивания Docker-образа, что полезно, когда долгая установка выглядит зависшей.
- **Обновления**: отдельная утилита [🔄 asc-updater](updater.md) — автообновления (можно отключить), каналы stable/beta, откат; при установке показывает настройки по умолчанию и спрашивает: установить с ними или изменить.

## 🔗 Связанные задачи

DMN-001…DMN-020 в [ROADMAP.md](../../../asc-platform/ROADMAP.md); GRW-005 в [ROADMAP-GROWTH.md](../../../asc-platform/ROADMAP-GROWTH.md).
