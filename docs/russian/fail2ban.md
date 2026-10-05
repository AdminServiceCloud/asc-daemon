# 🚫 fail2ban (демон)

> 🌍 **Язык:** Русский · [🇬🇧 English version](../english/fail2ban.md)

## 📌 Описание

Модуль `fail2ban` ставит на ноду [fail2ban](https://github.com/fail2ban/fail2ban) и управляет им: банит адреса, которые подбирают пароль SSH, ломятся на веб-сервер сканерами или повторяют нарушения. Баны исполняются через **nftables** в собственной таблице fail2ban `inet f2b-table`, рядом с `inet asc` демона ([🛡️ firewall](firewall.md)), так что они не перезаписывают друг друга.

Демон владеет ровно одним файлом — `/etc/fail2ban/jail.d/asc.local`. Файлы оператора `jail.local` и остальные `jail.d` не изменяются. Всё работает автономно через `asc fail2ban …`; платформа AdminService.Cloud пользуется тем же API ([🧩 node-modules](../../../asc-platform/docs/features/node-modules.md)). Команде нужен работающий демон. fail2ban управляется только системным (root) демоном.

## 🎯 Сценарии использования

- 🧰 `sudo asc fail2ban install` ставит пакет (apt / dnf), включает службу, пишет конфигурацию и запускает jail `sshd`; лог идёт потоком.
- 🔑 Пять неудачных входов по SSH за десять минут банят адрес на час (значения по умолчанию); бан растёт для повторных нарушителей. `asc fail2ban enable recidive` включает jail, который ловит адреса, банящиеся снова и снова, и банит их надолго на всех портах.
- 🌐 Если установлен [веб-сервер](webserver.md), можно включить jail'ы `nginx-http-auth`, `nginx-botsearch` и `nginx-limit-req`; они читают логи, которые собственный nginx демона пишет в `/var/log/asc/webserver/`.
- 🛡️ `asc fail2ban settings --add-ignore 203.0.113.0/24` гарантирует, что офисная сеть никогда не окажется в бане.
- 🎛️ `asc fail2ban tune sshd --maxretry 3 --bantime 1d` переопределяет один jail; `asc fail2ban settings --bantime -1` банит навсегда.
- 🔓 `asc fail2ban unban 198.51.100.4` освобождает адрес, который коллега заблокировал по ошибке; `asc fail2ban ban 198.51.100.9 --jail sshd` банит адрес вручную; `asc fail2ban bans` показывает, кто забанен и до какого времени.
- 🧪 Сломанное переопределение отклоняется: файл проверяется `fail2ban-client -t` до загрузки, прежняя версия остаётся.

## 🏗️ Техническое решение

Код: `src/daemon/fail2ban/` — `model.rs` (настройки, jail'ы, каталог, валидация), `config.rs` (рендер `asc.local`), `client.rs` (обёртка над `fail2ban-client` и разбор его вывода), `mod.rs` (менеджер).

### Установка и удаление

`InstallFail2banStream` отдаёт поток `log | done | error`, как установка веб-сервера: `apt-get install fail2ban` или `dnf install fail2ban`, затем пишется конфигурация, служба включается и запускается. `UninstallFail2ban(purge)` останавливает службу и удаляет пакет и `asc.local`; с `purge` — ещё и состояние демона.

### Конфигурация

- **Значения по умолчанию** (`[DEFAULT]`): `bantime` (`1h`), `findtime` (`10m`), `maxretry` (`5`), `bantime.increment` (растущий бан для повторных нарушителей), `ignoreip` (loopback и список оператора) и `banaction = nftables-multiport` / `banaction_allports = nftables-allports`. Время — в синтаксисе fail2ban: `90`, `10m`, `1h`, `1d`, `1w`; `-1` банит навсегда.
- **Jail'ы**: `sshd` (включён по умолчанию), `recidive`, `nginx-http-auth`, `nginx-botsearch`, `nginx-limit-req`. У каждого: `enabled`, `maxretry`, `bantime`, `findtime`, `port`, `logpath`. Nginx-jail'ы есть, только пока установлен модуль `webserver`; пути логов по умолчанию берутся у него.
- Всё, что вводит оператор, проверяется (время, числа, списки портов, абсолютные пути логов, адреса), так что значение не может начать новую строку конфига.
- Любое изменение проходит одни и те же шаги: рендер, запись (с сохранением прежнего файла), `fail2ban-client -t`, и — если fail2ban отказал — возврат прежнего файла и прежней модели. Затем `fail2ban-client reload`.
- **Reload может оставить jail без бан-действия**, когда заменяется действие, которым он пользовался (свежая установка: стоковый конфиг банит через `nftables`, наш — через `nftables-multiport`): jail считает неудачи и никого не банит. После каждого reload демон проверяет настраиваемые им jail'ы и перезапускает fail2ban, если один потерял действие; если и это не помогло, проблема попадает в `last_error`, а не скрывается. Баны переживают перезапуск: fail2ban восстанавливает их из своей базы.

### Управление

Через официальный клиент: `status`, `status <jail>`, `get <jail> banip --with-time`, `get <jail> actions`, `set <jail> banip|unbanip <ip>`. Вывод разбирается в типизированные сообщения; неожиданный формат становится ошибкой, а не молчаливым пустым списком. `fail2ban-client` — это запуск Python на каждый вызов, поэтому статусы jail'ов переиспользуются 4 с и сбрасываются после любого изменения. Бан исполняет своё nftables-действие внутри fail2ban асинхронно — вступает в силу примерно через секунду.

### API (`Fail2banService`)

| RPC | Что делает |
|---|---|
| `GetFail2ban` | Установлен ли, запущен ли, версия, значения по умолчанию, каждый известный jail с переопределениями, доступен ли он, запущен ли, и его счётчики |
| `InstallFail2banStream`, `UninstallFail2ban` | Установка с потоком лога, удаление |
| `UpdateFail2banSettings`, `UpsertFail2banJail` | Правка `asc.local` |
| `ListFail2banBans` | Забаненные адреса: IP, jail, с какого времени, до какого |
| `BanFail2banIp`, `UnbanFail2banIp` | Ручной бан и разбан (один адрес, не сеть) |

Capability в `GetStatus`: `fail2ban`. Все вызовы — только от root. Те же операции доступны как REST под `/v1/fail2ban` для CLI.

### CLI

`asc fail2ban status | install | uninstall | settings | jails | enable | disable | tune | bans | ban | unban`. Справочник команд: <https://docs.adminservice.cloud/ru/commands/fail2ban>.

## 🔗 Связанные задачи

| ID | Что |
|---|---|
| DMN-150 | Модуль: установка, конфигурация, jail'ы, баны |
| DMN-151 | CLI, переводы, зеркало документации |

См. также: [🛡️ firewall](firewall.md), [🌐 webserver](webserver.md).
