# 📦 Поддержка ASC в репозитории

> 🌍 **Язык:** Русский · [🇬🇧 English version](../english/repository-support.md)

## 📌 Описание

Пакет ASC в репозитории описывает одно приложение через `asc.yaml`, его настраиваемые параметры через `asc.settings.yaml`, либо несколько приложений через `asc.stack.yaml`.

## 🎯 Сценарии

### Одно Docker-приложение

Создайте в корне `asc.yaml`:

```yaml
name: example-web
version: 1.0.0
type: docker
title: Example web application
description: A small ASC package
settings: ./asc.settings.yaml
runtime:
  image: ghcr.io/acme/example-web:1.0.0
healthcheck:
  http: /health
```

Добавьте `asc.settings.yaml` для параметров, которые может менять оператор. Environment-переменные, порты и тома описываются здесь, а не в `asc.yaml`.

```yaml
quota: { max_cpu: 1, max_ram: 512M, max_disk: 2G }
settings:
  - key: http_port
    type: ports
    default: [8080]
    container: 3000
    limits: { min: 1024, max: 65535 }
    env: PORT
  - key: data
    type: volumes
    default: [/app/data]
  - key: admin_password
    type: secret
    required: true
    env: ADMIN_PASSWORD
```

### Несколько приложений

В корне разместите `asc.stack.yaml`, а манифесты каждого приложения — в указанных каталогах:

```yaml
name: example-stack
version: 1.0.0
apps:
  - name: database
    path: ./database
  - name: web
    path: ./web
    depends_on: [database]
  - name: metrics
    path: ./metrics
    optional: true
```

`asc install example-stack` ставит обязательные приложения в порядке зависимостей. `asc install example-stack/metrics` ставит выбранный optional-компонент и его зависимости.

## 🏗️ Техническое решение

- В корне должен быть ровно один entry point: `asc.yaml` для одного приложения или `asc.stack.yaml` для стека.
- `name`, `version` и `type` обязательны в `asc.yaml`; Docker-пакету нужен `runtime.image` или `runtime.image-build`, native-пакету — `runtime.start`.
- Типы настроек: `string`, `number`, `boolean`, `enum`, `secret`, `ports`, `volumes`. Настройки с `env` попадают в environment приложения.
- Значение `ports` — порт хоста; `container` задаёт порт внутри контейнера; `protocol` — `tcp`, `udp` или `both`.
- `depends_on` ссылается на имена этого же стека; несуществующие зависимости и циклы отклоняются.

Перед публикацией проверьте файлы по [схемам манифестов](https://github.com/AdminServiceCloud/registry/tree/main/schema).

## 🔗 Связанные задачи

DMN-003, DMN-017, DMN-030, DMN-052, DMN-057 в [ROADMAP.md](../../../asc-platform/ROADMAP.md).
