# 🤝 CONTRIBUTING — как контрибьютить в asc-daemon

Спасибо за интерес к проекту! Этот документ — правила участия в разработке демона.

> 🌍 До публичного релиза документация ведётся на русском; перед релизом будет переведена на английский (задача CORE-011).

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

## 🔀 Процесс изменений

1. **Найди или создай задачу** в [ROADMAP](../asc-platform/ROADMAP.md) (префикс `DMN-*`). Любая работа начинается с задачи и дока в `docs/` — см. [AGENTS.md](AGENTS.md).
2. Форкни/создай ветку от `main`: `feat/dmn-003-package-manager`, `fix/…`, `docs/…`.
3. Пиши код и тесты. Перед коммитом — `task check` (clippy без warnings + fmt + тесты).
4. Открой Pull Request по шаблону. CI должен быть зелёным.
5. Ревью → merge. Squash-merge, заголовок PR — по Conventional Commits.

## 📏 Правила кода

- **Стиль**: `cargo fmt` (без отклонений), `cargo clippy -- -D warnings`.
- **Коммиты**: [Conventional Commits](https://www.conventionalcommits.org/) — `feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`; область — модуль (`feat(pkg): …`).
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

Проект распространяется под [MIT-лицензией](LICENSE): можно распространять, модифицировать и использовать коммерчески, но **обязательно сохранение авторства** — Omar El Sayed ([@statebyte](https://github.com/statebyte)), проект AdminService.Cloud, [Anytecture Software](https://anytecture.com). Отправляя PR, ты соглашаешься лицензировать свой вклад на тех же условиях.
