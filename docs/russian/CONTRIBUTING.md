# 🤝 CONTRIBUTING — как контрибьютить в asc-daemon

Спасибо за интерес к проекту! Этот документ — правила участия в разработке демона.

> 🌍 English version: [../../CONTRIBUTING.md](../../CONTRIBUTING.md)

## 🚀 Быстрый старт

```bash
git clone https://github.com/AdminServiceCloud/asc-daemon
cd asc-daemon
task dev            # запустить демон локально (debug)
task check          # линт + тесты — обязательно перед PR
```

Требования: Rust (stable, через rustup), [Task](https://taskfile.dev), для кросс-сборки — [cross](https://github.com/cross-rs/cross). Основные платформы разработки — **Debian/Ubuntu** (остальные дистрибутивы и ОС — по мере развития проекта).

## 📂 Структура репозитория

```
asc-daemon/
├── proto/          # 📜 proto-контракты API (источник правды, линкуются платформой)
├── src/            # 🦀 все исходники демона
│   ├── cli/        # CLI (asc)
│   ├── daemon/     # сервис: api, tunnel, apps, pkg, mcp, backup, monitor, sftp, db, console, scheduler, config
│   └── updater/    # asc-updater
├── skills/         # 🧠 Agent Skills для AI-агентов
├── docs/           # 📚 документация модулей
└── Taskfile.yml    # 🛠️ команды разработки
```

## 🌿 Модель веток

| Ветка | Канал | Назначение |
|-------|-------|------------|
| `main` | **stable** | Только релизы: `dev` вливается сюда при релизе и тегируется `v<версия>`. Прямые пуши и PR запрещены. |
| `dev`  | **beta**   | Интеграционная ветка — **все контрибьюты идут сюда**. Из неё собирается beta-канал обновлений. |

**Каждый PR открывается в `dev`.** PR в `main` будут перенацелены или закрыты. Фичевые ветки создаются от `dev` и называются `feat/…`, `fix/…`, `docs/…` (по типам коммитов).

## 🔀 Процесс изменений

1. **Найди или создай задачу** в [ROADMAP](../../../asc-platform/ROADMAP.md) (префикс `DMN-*`) и/или GitHub Issue. Любая работа начинается с задачи и дока в `docs/` — см. [AGENTS.md](../../AGENTS.md).
2. Форкни и создай ветку от `dev`: `feat/dmn-003-package-manager`, `fix/…`, `docs/…`.
3. Пиши код и тесты. Перед коммитом — `task check` (clippy без warnings + fmt + тесты).
4. Открой Pull Request **в `dev`** по шаблону. CI должен быть зелёным.
5. Ревью → merge. Squash-merge, заголовок PR — по Conventional Commits.

## 🧭 От клонирования до первого PR (по Issue)

```bash
# 0. Выбери Issue (или сначала открой свой и дождись 👍) — допустим, это #42.
#    Форкни репозиторий на GitHub (кнопка "Fork"), затем:

# 1. Клонируй свой форк и подключи upstream
git clone git@github.com:<твой-логин>/asc-daemon.git
cd asc-daemon
git remote add upstream git@github.com:AdminServiceCloud/asc-daemon.git

# 2. Начинай от свежего dev (никогда не от main)
git fetch upstream
git switch dev
git reset --hard upstream/dev

# 3. Создай фичевую ветку под Issue
git switch -c fix/dmn-021-console-reconnect

# 4. Код + тесты, затем локальная проверка
task check

# 5. Коммит: английский, Conventional Commits, со ссылкой на Issue
git add -A
git commit -m "fix(console): reconnect attach session after daemon restart (#42)"

# 6. Держи ветку в актуальном состоянии относительно dev (rebase, не merge)
git fetch upstream
git rebase upstream/dev

# 7. Запушь ветку в свой форк и открой PR в dev
git push -u origin fix/dmn-021-console-reconnect
# На GitHub: "Compare & pull request" → base repository: AdminServiceCloud/asc-daemon, base: dev
# В описании PR добавь "Closes #42" — Issue закроется при merge.
# Или через GitHub CLI:
gh pr create --base dev --fill
```

После замечаний ревью: пуш новых коммитов в ту же ветку — PR обновится автоматически (при merge они будут squash-нуты).

## 📏 Правила кода

- **Стиль**: `cargo fmt` (без отклонений), `cargo clippy -- -D warnings`.
- **Коммиты**: только на английском, [Conventional Commits](https://www.conventionalcommits.org/) с типами из [conventional-commit-types](https://github.com/pvdlg/conventional-commit-types) — `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`, `revert`; область — модуль (`feat(pkg): …`).
- **Proto**: меняя `proto/` — только обратно совместимо (новые поля — optional, старые не удалять/не перенумеровывать); breaking-изменения — через новую версию пакета и обсуждение в issue.
- **Локализация**: пользовательские сообщения CLI — только через систему переводов (ключи EN + RU); debug/лог-сообщения — на английском без переводов.
- **Платформы**: код не должен ломать сборку под x86_64/ARM64/ARMv7; специфичное для дистрибутива — за абстракцией (сначала Debian/Ubuntu, дальше — остальные).
- **Тесты**: новая логика — с юнит-тестами; фиксы багов — с регрессионным тестом.

## 🐛 Issues

- Баг: шаблон bug report — версия (`asc --version`), ОС, шаги воспроизведения, логи (`journalctl -u asc`).
- Фича: сначала issue с обсуждением — возможно, она уже в roadmap.

## ⚙️ CI (GitHub Actions)

- `ci.yml` — на каждый PR: fmt, clippy, тесты, сборка.
- `release.yml` — на тег `v*`: кросс-сборка всех архитектур, чексуммы, GitHub Release.

## 📄 Лицензия

Проект распространяется под [MIT-лицензией](../../LICENSE): можно распространять, модифицировать и использовать коммерчески, но **обязательно сохранение авторства** — Omar El Sayed ([@statebyte](https://github.com/statebyte)), проект AdminService.Cloud, [Anytecture Software](https://anytecture.com). Отправляя PR, ты соглашаешься лицензировать свой вклад на тех же условиях.
