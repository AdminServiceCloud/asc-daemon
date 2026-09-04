# 🗂️ Создание своего registry

> 🌍 **Язык:** Русский · [🇬🇧 English version](../english/custom-registry.md)

## 📌 Описание

Registry ASC — статическая иерархия JSON-файлов. Корневой `registry.json` ссылается на файлы категорий, а те содержат пакеты и подкатегории. Демон читает registry из GitHub raw или любого HTTPS-сайта.

## 🎯 Сценарии

### Минимальный registry

```text
my-registry/
├── registry.json
└── categories/
    └── web.json
```

```json
// registry.json
{
  "name": "acme-registry",
  "title": "Acme Registry",
  "format_version": 1,
  "categories": [{ "name": "web", "index": "categories/web.json" }]
}
```

```json
// categories/web.json
{
  "category": "web",
  "packages": [{
    "name": "example-web",
    "type": "app",
    "description": "Example web application",
    "source": { "git": "https://github.com/acme/example-web" }
  }]
}
```

### Публикация в GitHub

Закоммитьте файлы в публичный репозиторий и добавьте raw URL каталога на сервере:

```bash
asc source add https://raw.githubusercontent.com/acme/my-registry/main --name acme
asc update
asc search example-web
asc install example-web --source acme
```

### Публикация на своём HTTPS-сайте

Скопируйте каталог в `/var/www/asc-registry` и отдайте его как статику. Минимальный Nginx-блок:

```nginx
server {
  listen 443 ssl;
  server_name packages.example.com;
  root /var/www/asc-registry;
  location / { try_files $uri =404; }
}
```

Затем подключите базовый URL без `registry.json` в конце:

```bash
sudo asc source add https://packages.example.com --name acme
asc update
asc search example-web
```

## 🏗️ Техническое решение

- `registry.json` требует `name`, `format_version` и `categories`; индекс категории — `category` и `packages`.
- Относительные пути `index` считаются от корня registry. `source.git` указывает репозиторий пакета, `source.path` — каталог его манифеста.
- Тип пакета: `app` для `asc.yaml` или `stack` для `asc.stack.yaml`.
- Registry должен быть публично доступен по HTTPS. Проверяйте JSON по [схемам registry](https://github.com/AdminServiceCloud/registry/tree/main/schema).
- `sudo asc source add` создаёт источник для всех пользователей сервера; без sudo он доступен только текущему пользователю.

### Источники под управлением платформы (DMN-083)

`SourceService` — API-эквивалент `sudo asc source add/remove`: то, как платформа AdminService.Cloud выкладывает реестры организации на подключённую ноду без доступа по SSH.

- `ListSources` отдаёт только **системный** список (`/etc/asc/sources.toml`); собственные пользовательские источники ноды (`~/.config/asc/sources.toml`) этому API не видны и остаются под управлением CLI.
- `ReplaceSources` — **идемпотентная полная замена**, а не add/remove по одному: запрос несёт весь желаемый список источников организации, и системный список демона становится ровно им — включая удаления. Это важно, как только нода переходит под управление платформы: **источник, добавленный вручную через `sudo asc source add` на этой ноде, будет удалён следующим пушем `ReplaceSources`**, если его нет в списке платформы. Валидация — та же, что у `asc source add` (только `https://`/`file://`-URL, зарезервированное имя `git` отвергается), плюс проверка на повтор имён внутри одного запроса.
- Оба RPC требуют лишь валидного bearer-токена API у вызывающего (тот же уровень доверия, что у `RemoveApp`/`RebootSystem`) — демон не знает про организации и проекты, авторизация по конкретному реестру — на уровне фасада платформы.
- Анонсируется через `capabilities` в `GetStatus` (DMN-076) как `"sources"` — платформа, говорящая со старым демоном, видит отсутствие capability и пропускает пуш вместо `UNIMPLEMENTED`.

## 🔗 Связанные задачи

DMN-003, DMN-057, DMN-076, DMN-083, REG-001, REG-003, REG-005 в [ROADMAP.md](../../../asc-platform/ROADMAP.md).
