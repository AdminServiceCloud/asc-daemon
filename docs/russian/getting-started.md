# 🚀 Начало работы

> 🌍 **Язык:** Русский · [🇬🇧 English version](../english/getting-started.md)

## 📌 Описание

Установите демон `asc` на сервер и проверьте systemd-сервис. Основные целевые дистрибутивы — Debian и Ubuntu; поддерживаются x86_64, ARM64 и ARMv7.

## 🎯 Сценарии

### Интерактивная установка

Запустите установщик от sudo. Он установит `asc-updater`, который скачает демон и спросит язык и настройки обновлений. Если Docker отсутствует, он предложит его установить для контейнерных приложений.

```bash
curl -fsSL https://raw.githubusercontent.com/AdminServiceCloud/asc-daemon/main/install.sh | sudo bash
```

### Автоматическая установка

Для CI, скриптов и провижининга используйте `--silent`: принимаются все настройки по умолчанию.

```bash
curl -fsSL https://raw.githubusercontent.com/AdminServiceCloud/asc-daemon/main/install.sh | sudo bash -s -- --silent
```

### Проверка

```bash
asc status
asc service status
docker --version
```

`asc status` показывает версию, состояние сервиса и сводку приложений. Управляйте systemd-сервисом через `sudo asc service start|stop|restart|status`.

## 🏗️ Техническое решение

- Установка требует root: она устанавливает бинарные файлы, создаёт `/asc`, `/etc/asc/config.toml` и systemd-юнит.
- `asc-updater` отвечает за установку, обновления, каналы и откат.
- Docker нужен только пакетам `type: docker`; native и utility-пакеты обходятся без него.
- Изменить язык позднее можно командой `sudo asc config lang en` или `sudo asc config lang ru`.

## 🔗 Связанные задачи

DMN-001, DMN-014, DMN-057 в [ROADMAP.md](../../../asc-platform/ROADMAP.md).
