# 🌐 Веб-сервер (демон)

> 🌍 **Язык:** Русский · [🇬🇧 English version](../english/webserver.md)

## 📌 Описание

Модуль `webserver` ставит на ноду веб-сервер и управляет им. Первый движок — **nginx**. Демон публикует через него приложения на доменах (reverse proxy), сам выпускает и продлевает сертификаты Let's Encrypt, принимает готовые сертификаты и умеет пропускать реальные IP посетителей из-за Cloudflare. Всё работает автономно через `asc web …`. Платформа AdminService.Cloud пользуется тем же API и раздаёт сайты как desired state ([🌐 web-server](../../../asc-platform/docs/features/web-server.md)).

Веб-сервером управляет только системный (root) демон.

## 🎯 Сценарии использования

- 🧰 `sudo asc web install --mode system` ставит nginx из репозитория nginx.org (или пакет дистрибутива, если nginx.org не собирает под этот релиз) либо подхватывает уже установленный. `--mode docker` поднимает контейнер `asc-webserver`.
- 🚀 `asc web site add grafana.example.com --app grafana --port 3000 --tls letsencrypt` — через минуту `https://grafana.example.com` работает, а сертификат потом продлевается сам.
- 🔗 `asc web site add api.example.com --to 127.0.0.1:8080` проксирует на сервис, который не является приложением ASC.
- ☁️ `asc web site add … --cloudflare` передаёт в приложение настоящие IP посетителей, а не адреса Cloudflare.
- 🧪 `asc web test` показывает вывод `nginx -t` для того, что было бы применено сейчас; `asc web reload` перерисовывает конфиг, проверяет его и перезагружает nginx.
- 🛡️ Платформа прислала сайт с опечаткой в `extra_server`. `nginx -t` падает, демон применяет остальные сайты, а для этого оставляет **прошлую рабочую версию** и помечает его `error` с текстом ошибки.

## 🏗️ Техническое решение

Код — `src/daemon/webserver/`:

| Файл | Что делает |
|---|---|
| `mod.rs` | менеджер `WebServer`: хранилища, операции API, фоновый проход |
| `model.rs` | настройки, сайты, статусы и валидация всего, что попадает в директивы |
| `render.rs` | генерация `nginx.conf`, сниппетов и файлов сайтов (чистые функции) |
| `apply.rs` | стейджинг → `nginx -t` → подмена → reload, изоляция сломанного сайта |
| `engine.rs` | установка и управление nginx в режимах `system` и `docker` |
| `acme.rs` | Let's Encrypt (HTTP-01) с самопроверкой домена |
| `cert.rs` | срок, издатель, SAN сертификата; проверка пары ключ–сертификат |
| `cloudflare.rs` | диапазоны Cloudflare для `real_ip` |

API — `src/daemon/api/webserver.rs` (gRPC `WebServerService` и REST для CLI).

### 🧰 Режимы установки

| Режим | Установка | Управление процессом |
|---|---|---|
| `system` | apt (Debian/Ubuntu): репозиторий nginx.org (`signed-by` на ASCII-ключ, pin на origin nginx.org); если для релиза нет пакетов — пакет дистрибутива. dnf/yum (семейство RHEL): репозиторий nginx.org с откатом на пакет дистрибутива. Уже стоящий nginx **подхватывается**: исходный `nginx.conf` один раз сохраняется в `nginx.conf.asc-orig` | `systemctl reload-or-restart nginx`, статус из `systemctl is-active` |
| `docker` | Контейнер `asc-webserver` из `nginx:stable-alpine` (образ настраивается), `network_mode: host`, `restart: unless-stopped`, метка `asc.managed=webserver`; только bollard, без `docker` CLI | `SIGHUP` контейнеру, `nginx -t` через exec |

**Подхват (`system`).** Из исходного `nginx.conf` сохраняются `user` и `pid` (последний обязан совпадать с `PIDFile` systemd-юнита), include динамических модулей Debian, `conf.d/*.conf` и `sites-enabled/*`. Чужие файлы демон не правит. Если там уже есть `default_server` на `:80` или `:443` (как у Debian `sites-enabled/default`), свой default-сервер он просто не генерирует. При удалении подхваченный nginx получает исходный `nginx.conf` обратно и остаётся установленным. Для SELinux в режиме Enforcing включается `httpd_can_network_connect`. Про активный ufw/firewalld демон только подсказывает, сам файрвол не трогает.

**Docker.** До запуска демон проверяет, что порты 80/443 свободны. nginx — PID 1 (entrypoint образа заменён): скрипты образа переписывают неиспользуемый `default.conf`, а `SIGHUP`, пришедший во время их работы, попал бы в shell. Конфиг передаётся ключом `-c /etc/asc/webserver/nginx.conf`, а не монтированием одного файла: атомарная подмена через `rename` была бы не видна в контейнере. Каталоги `/etc/asc/webserver`, `/var/www/asc-acme` и `/var/lib/asc/webserver/acme/certs` монтируются read-only **по тем же путям**, что на хосте, поэтому сгенерированный конфиг одинаков в обоих режимах.

### ⚙️ Настройки — `/etc/asc/webserver/webserver.toml`

```toml
mode = "system"             # system | docker; пишет install
image = "nginx:stable-alpine"
worker_processes = "auto"
worker_connections = 4096
keepalive_timeout = 65
client_max_body_size = "64m"
gzip = true
gzip_level = 5
server_tokens = false
http2 = true
tls_protocols = ["TLSv1.2", "TLSv1.3"]
hsts = false
cloudflare_real_ip = false  # true — доверять Cloudflare во всех сайтах (http-уровень)
access_log = true
acme_email = "ops@example.com"
acme_directory = ""         # пусто — Let's Encrypt production
custom_main = ""            # вставляется в main-контекст
custom_http = ""            # вставляется в http {}

[host]                      # факты о хосте, снятые при установке
user = "www-data"
pid = "/run/nginx.pid"
```

Меняются через API (`UpdateWebServerSettings`): новые настройки сначала проходят `nginx -t` и сохраняются только при успехе.

### 🧱 Файлы

```
/etc/asc/webserver/
├── webserver.toml
├── nginx.conf                          # system: пишется в /etc/nginx/nginx.conf
├── http.d/custom.conf                  # custom_http
├── snippets/acme.conf                  # location ^~ /.well-known/acme-challenge/
├── snippets/proxy.conf                 # X-Real-IP, X-Forwarded-*
├── snippets/cloudflare-realip.conf
├── sites/<id>.conf
└── certs/<id>/{fullchain.pem,privkey.pem}      # переданные сертификаты, ключ 0600
/var/lib/asc/webserver/
├── sites.json                          # сайты (desired state), 0600
├── status.json                         # статусы сайтов и последнее применение
├── cloudflare.json                     # последний скачанный список диапазонов
└── acme/
    ├── account-<hash>.json             # ACME-аккаунт на каждый каталог, 0600
    └── certs/<id>/{fullchain.pem,privkey.pem}
/var/www/asc-acme/.well-known/acme-challenge/   # webroot HTTP-01, читается воркером nginx
```

`nginx.conf` содержит `default_server` для `:80` (ACME-сниппет, остальное — `return 444`) и для `:443` (`ssl_reject_handshake on`, nginx 1.19.4+), чтобы чужие имена не получали первый попавшийся сайт. Синтаксис зависит от версии: `http2 on;` с 1.25.1, до неё — `listen … http2`. `listen [::]` генерируется, только если у хоста есть IPv6.

### 🔁 Применение конфигурации

1. Разрешить каждый сайт: адреса upstream (приложение → `127.0.0.1:<хост-порт>`), путь к сертификату, конфликты имён. Локальные сайты занимают имена первыми, дальше — по id, поэтому при конфликте отклоняется всегда один и тот же сайт.
2. Отрендерить полный набор файлов в `/etc/asc/webserver/.staging/` и выполнить `nginx -t -c .staging/nginx.conf`.
3. Если вывод называет файл конкретного сайта или его сертификат, этот сайт откатывается на прошлый рабочий файл, а если его нет — исключается. Проверка повторяется. Ошибка в общем конфиге отменяет всё, живые файлы не трогаются.
4. Отрендерить набор заново с живыми путями, записать каждый файл через временный файл и `rename`, удалить файлы исчезнувших сайтов, выполнить reload.
5. Записать статусы: `applied`, `error` (с выводом `nginx -t`), `disabled`, `pending` (веб-сервер не установлен).

Применения сериализуются мьютексом. Сайт с Let's Encrypt, у которого ещё нет сертификата, рендерится только на `:80`. Как только сертификат выпущен, применение повторяется и добавляет `:443` и редирект.

### 🧩 Сайт

```text
Site
  id              [a-z0-9][a-z0-9_.-]{0,63} (у платформы — id домена)
  server_names    обычные и wildcard (*.example.com) имена, без regex
  managed_by      отсутствует — локальный; "platform" — сайты платформы
  disabled
  upstream        servers[]: app{app, port} | address "host:port"; weight, backup,
                  max_fails, fail_timeout, down; balance: round_robin|least_conn|ip_hash|hash;
                  hash_key; keepalive; tls (HTTPS до upstream)
  tls             mode none|acme|provided; certificate_pem/private_key_pem (только provided,
                  через API не возвращаются); redirect_http, hsts, http2
  proxy           websocket, client_max_body_size, таймауты connect/read/send,
                  request_headers[], response_headers[], upstream_host, no_buffering
  real_ip         off | cloudflare
  extra_server, extra_location     куски текста, вставляемые в server {} и location / {}
  raw_config      полный текст сайта — рендерится как есть
```

Участник `app{app, port}` — id, имя или uuid приложения и порт контейнера. Демон разрешает его в опубликованный хост-порт. Фоновый проход замечает, что приложение пересоздано с другим портом, и перерисовывает сайт. `backup` молча отбрасывается для `ip_hash`/`hash`: nginx его там не допускает.

Значения заголовков не могут содержать кавычки, `;`, `{`, `}`, обратный слэш и управляющие символы. `extra_*`, `raw_config` и `custom_*` — произвольные директивы nginx, то есть по возможностям это доступ root к ноде: платформа выдаёт их только тем, у кого есть право на управление нодой.

`ReplaceSites(managed_by, sites)` заменяет **только** сайты с тем же `managed_by`. Локальные сайты CLI и платформенные друг друга не затирают, а один и тот же id у разных владельцев отклоняется.

### 🔐 Let's Encrypt (ACME)

- Крейт `instant-acme` на rustls + ring (тот же TLS-стек, что у API), проверка **HTTP-01** через webroot.
- **Самопроверка.** Перед заказом каждое имя проверяется. Не резолвится — `pending_dns` («создайте A/AAAA-запись»). Отвечает не эта нода (другой ответ или код) — `pending_dns` с адресами, куда оно указывает. В обоих случаях заказ не создаётся, и лимиты Let's Encrypt не тратятся. Если подключиться не удалось (hairpin NAT), проверка неубедительна: заказ идёт, решает Let's Encrypt.
- Состояния: `pending_dns` → `issuing` → `active` → `expiring` (меньше 30 дней) / `error` (текст ACME).
- Backoff после неудач: 1 ч, 2 ч, 4 ч … до 24 ч. `asc web site renew <id>` выпускает сразу, игнорируя backoff.
- Ключ сертификата генерируется на ноде и не покидает её. Аккаунт создаётся один раз на каждый каталог ACME, контакт — `acme_email`.
- `acme_directory` переопределяется: staging Let's Encrypt для тестов или другой ACME-провайдер.

### 🩺 Проверки здоровья (DMN-126)

У upstream сайта может быть активная проверка: `health_check` с видом `tcp` (подключение) или `http` (GET пути, ожидаемый код или любой 2xx/3xx), интервалом (5 с), таймаутом (2 с), числом неудач до выхода (3) и успехов до возврата (2). Демон проверяет каждый сервер напрямую, а не через nginx, поэтому выведенный сервер продолжает проверяться и возвращается сам. Упавший сервер получает в upstream `down`, смена состояния применяется через `nginx -t` и reload не чаще раза в 3 секунды. Если упали все серверы, ни один не выводится: пустой upstream хуже, чем попытки nginx. Состояние хранится в памяти и отдаётся в статусе сайта (`upstream_health`: здоров ли, последняя ошибка, задержка). HTTPS-проверки не проверяют сертификат: серверы адресуются по IP.

`ProbeTcp` — разовое TCP-подключение с ноды к адресу: так платформа проверяет, что участник балансировщика на другой ноде доступен с ноды-балансера.

### 🔄 Фоновый проход

Раз в 10 минут, а также сразу после изменения сайтов с Let's Encrypt:

- раз в сутки обновить диапазоны Cloudflare;
- проверить, не сменился ли хост-порт у приложений из upstream;
- выпустить или продлить сертификаты, у которых нет файла, сменились имена или осталось меньше 30 дней, если backoff позволяет.

### ☁️ Cloudflare real IP

`snippets/cloudflare-realip.conf` — `set_real_ip_from` для всех диапазонов Cloudflare, `real_ip_header CF-Connecting-IP`, `real_ip_recursive on`. Список раз в сутки обновляется с `https://www.cloudflare.com/ips-v4` и `/ips-v6`. Каждая строка проверяется как CIDR: любая невалидная строка отбрасывает весь ответ, чтобы страница ошибки или captive portal не стали списком доверенных прокси. В бинарник вшит запасной список. Сниппет подключается в `http {}` (`cloudflare_real_ip = true`) или в `server {}` отдельного сайта.

### 📡 API — `WebServerService`

| RPC | REST (CLI) | Назначение |
|---|---|---|
| `GetWebServer` | `GET /v1/webserver` | установлен ли, режим, версия, работает ли, настройки, последняя ошибка, дата списка Cloudflare, есть ли nginx и Docker на хосте |
| `InstallWebServerStream` | `POST /v1/webserver/install` `{mode}` | установка или подхват; gRPC — поток строк, REST — `{log, webserver}` |
| `UninstallWebServer` | `DELETE /v1/webserver?purge=` | удаление (`purge` — вместе с конфигами, сайтами и сертификатами) |
| `UpdateWebServerSettings` | `PUT /v1/webserver/settings` | новые настройки → `nginx -t` → применение |
| `TestWebServerConfig` / `ReloadWebServer` | `POST /v1/webserver/test` / `…/reload` | `nginx -t` без применения / полное применение |
| `GetWebServerFiles` | `GET /v1/webserver/files` | итоговые файлы конфигурации |
| `ListSites` | `GET /v1/webserver/sites` | сайты со статусами |
| `ReplaceSites` / `UpsertSite` / `RemoveSite` | `PUT`/`DELETE /v1/webserver/sites/{id}` | desired state сайтов |
| `RenderSite` | — | отрендерить сайт без применения (превью для UI) |
| `RenewCertificate` | `POST /v1/webserver/sites/{id}/renew` | немедленный выпуск |
| `ProbeTcp` | — | TCP-подключение с ноды к адресу (DMN-126) |

Capability в `GetStatus` — `webserver`.

### ⌨️ CLI

```
asc web status
asc web install [--mode system|docker]
asc web uninstall [--purge]
asc web test
asc web reload
asc web site list
asc web site add <имя>... (--app <id> --port <порт> | --to <host:port> [--to …]) [--balance round-robin|least-conn|ip-hash] [--health off|tcp|http] [--health-path /] [--id <id>] [--tls none|letsencrypt] [--cloudflare] [--no-websocket] [--max-body <размер>]
asc web site show <id>
asc web site remove <id>
asc web site renew <id>
```

Без запущенного демона команды работают in-process от root.

## 🔗 Связанные задачи

DMN-122, DMN-123, DMN-124, DMN-125, DMN-126 (health checks для балансировщика), DMN-067 (ACME для API демона) в [ROADMAP.md](../../../asc-platform/ROADMAP.md).
