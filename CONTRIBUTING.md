# 🤝 CONTRIBUTING — how to contribute to asc-daemon

Thank you for your interest in the project! This document describes the rules for taking part in daemon development.

> 🌍 Русская версия: [docs/russian/CONTRIBUTING.md](docs/russian/CONTRIBUTING.md)

## 🚀 Quick start

```bash
git clone https://github.com/AdminServiceCloud/asc-daemon
cd asc-daemon
task dev            # run the daemon locally (debug)
task check          # lint + tests — mandatory before a PR
```

Requirements: Rust (stable, via rustup), [Task](https://taskfile.dev), and [cross](https://github.com/cross-rs/cross) for cross-builds. Primary development platforms are **Debian/Ubuntu** (other distributions and OSes as the project evolves).

## 📂 Repository layout

```
asc-daemon/
├── proto/          # 📜 API proto contracts (source of truth, linked by the platform)
├── src/            # 🦀 all daemon sources
│   ├── cli/        # CLI (asc)
│   ├── daemon/     # service: api, tunnel, apps, pkg, mcp, backup, monitor, sftp, db, console, scheduler, config
│   └── updater/    # asc-updater
├── skills/         # 🧠 Agent Skills for AI agents
├── docs/           # 📚 module documentation
└── Taskfile.yml    # 🛠️ development commands
```

## 🌿 Branch model

| Branch | Channel | Purpose |
|--------|---------|---------|
| `main` | **stable** | Releases only: `dev` is merged here at release time and tagged `v<version>`. No direct pushes or PRs. |
| `dev`  | **beta**   | Integration branch — **all contributions land here**. The beta update channel builds from it. |

**Every PR targets `dev`.** PRs opened against `main` will be retargeted or closed. Feature branches are created from `dev` and named `feat/…`, `fix/…`, `docs/…` (matching the commit types).

## 🔀 Change workflow

1. **Find or create a task** in the [ROADMAP](../asc-platform/ROADMAP.md) (`DMN-*` prefix) and/or a GitHub Issue. Every piece of work starts with a task and a doc in `docs/` — see [AGENTS.md](AGENTS.md).
2. Fork and branch off `dev`: `feat/dmn-003-package-manager`, `fix/…`, `docs/…`.
3. Write code and tests. Before committing — `task check` (clippy with no warnings + fmt + tests).
4. Open a Pull Request **to `dev`** using the template. CI must be green.
5. Review → merge. Squash-merge; the PR title follows Conventional Commits.

## 🧭 From clone to your first PR (based on an Issue)

```bash
# 0. Pick an Issue to work on (or open one first and wait for a 👍) — say it is #42.
#    Fork the repository on GitHub (the "Fork" button), then:

# 1. Clone your fork and wire up the upstream remote
git clone git@github.com:<your-login>/asc-daemon.git
cd asc-daemon
git remote add upstream git@github.com:AdminServiceCloud/asc-daemon.git

# 2. Start from the freshest dev (never from main)
git fetch upstream
git switch dev
git reset --hard upstream/dev

# 3. Create a feature branch for the issue
git switch -c fix/dmn-021-console-reconnect

# 4. Code + tests, then verify everything locally
task check

# 5. Commit: English, Conventional Commits, reference the issue
git add -A
git commit -m "fix(console): reconnect attach session after daemon restart (#42)"

# 6. Stay current with dev while you work (rebase, do not merge)
git fetch upstream
git rebase upstream/dev

# 7. Push the branch to your fork and open a PR against dev
git push -u origin fix/dmn-021-console-reconnect
# On GitHub: "Compare & pull request" → base repository: AdminServiceCloud/asc-daemon, base: dev
# In the PR description add "Closes #42" so the issue closes on merge.
# Or with the GitHub CLI:
gh pr create --base dev --fill
```

After review feedback: push new commits to the same branch — the PR updates automatically (they are squashed on merge).

## 📏 Code rules

- **Style**: `cargo fmt` (no deviations), `cargo clippy -- -D warnings`.
- **Commits**: English only, [Conventional Commits](https://www.conventionalcommits.org/) with the types from [conventional-commit-types](https://github.com/pvdlg/conventional-commit-types) — `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`, `revert`; the scope is a module (`feat(pkg): …`).
- **Proto**: changes to `proto/` must be backward compatible (new fields are optional; never delete or renumber existing ones); breaking changes go through a new package version and an issue discussion.
- **Localization**: user-facing CLI messages only through the translation system (EN + RU keys); debug/log messages are English-only, untranslated.
- **Platforms**: code must not break the build on x86_64/ARM64/ARMv7; distribution-specific bits go behind an abstraction (Debian/Ubuntu first, the rest later).
- **Tests**: new logic ships with unit tests; bug fixes ship with a regression test.

## 🐛 Issues

- Bug: use the bug report template — version (`asc --version`), OS, reproduction steps, logs (`journalctl -u asc`).
- Feature: open an issue for discussion first — it may already be on the roadmap.

## ⚙️ CI (GitHub Actions)

- `ci.yml` — on every PR: fmt, clippy, tests, build.
- `release.yml` — on a `v*` tag: cross-builds for all architectures, checksums, GitHub Release.

## 📄 License

The project is distributed under the [MIT license](LICENSE): you may distribute, modify and use it commercially, but **attribution is mandatory** — Omar El Sayed ([@statebyte](https://github.com/statebyte)), the AdminService.Cloud project, [Anytecture Software](https://anytecture.com). By submitting a PR you agree to license your contribution under the same terms.
