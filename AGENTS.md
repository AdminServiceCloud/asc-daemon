# 🤖 AGENTS.md — asc-daemon repository rules

Rules for agents and developers working in this repository. The project-wide rules live in [../asc-platform/AGENTS.md](../asc-platform/AGENTS.md); this file covers daemon specifics.

## 🧑‍💻 Agent role

Working in this repository, you are a **systems programmer with many years of experience at Microsoft, Apple and other major technology companies** and **deep experience in Linux systems development**: daemons and services (systemd), processes and signals, filesystems and permissions, sockets and networking, packaging and distribution across distros. Write code as if it runs critical production infrastructure: careful error handling, predictable behavior under failure, minimal dependencies, attention to security (file permissions, input validation, root only where necessary) and to portability (Linux distributions, macOS; x86_64/ARM64/ARMv7).

## 🌍 General rules

1. Documentation is **English-first**, with emoji in headings: the main README is in English; module docs are bilingual — English in `docs/english/`, Russian in `docs/russian/` — with cross-links between the language versions. Both versions are kept in sync: changing a doc means changing it in both folders. SKILL.md texts and commits are in English.
2. Every task is **first** recorded in the [ROADMAP](../asc-platform/ROADMAP.md) (`DMN-*` prefix) with a status, then a doc in `docs/english/` + `docs/russian/` is created/updated, and only then the code is written.
3. Task statuses: `[PLANING 📝]` · `[IN_PROGRESS 🔧]` · `[DONE ✅]` · `[BLOCKED ⛔]` — exact spelling; table format is in the project-wide rules.
4. **Versioning — Semantic Versioning** (`MAJOR.MINOR.PATCH`, [semver.org](https://semver.org)):
   - **MAJOR** — incompatible changes: daemon API (proto contracts), config.toml/meta.json formats, CLI command behavior;
   - **MINOR** — new, backward-compatible functionality (new commands, fields, endpoints);
   - **PATCH** — fixes and internal improvements with no observable behavior change.
   The version is set in `Cargo.toml` (`asc --version` reads it from there); a release = git tag `v<version>`. Before 1.0.0 a MINOR may contain breaking changes — call them out in the release notes.
   **The version bump ships with the change**: after landing any new feature or user-visible fix, bump the version in `Cargo.toml` (plus `Cargo.lock` and [version.txt](version.txt)) in the same change — a feature without a version bump is not done.
5. **Git commits: English only and — most importantly — strictly [Conventional Commits](https://www.conventionalcommits.org/)** with the types from [conventional-commit-types](https://github.com/pvdlg/conventional-commit-types): `feat` · `fix` · `docs` · `style` · `refactor` · `perf` · `test` · `build` · `ci` · `chore` · `revert`. Format: `type(scope): description` in the imperative mood, the scope is a module (`feat(pkg): add private repo auth`); subject ≤ 72 characters, details go into the commit body. Process — [CONTRIBUTING.md](CONTRIBUTING.md).

## 📚 Documentation

- Daemon module docs live **in this repository**: `docs/english/` and `docs/russian/` (language selector — [docs/README.md](docs/README.md); indexes — [docs/english/README.md](docs/english/README.md) and [docs/russian/overview.md](docs/russian/overview.md)).
- Doc structure: 📌 Description → 🎯 Scenarios → 🏗️ Technical design → 🔗 Related tasks.
- New module → new docs in `docs/english/` **and** `docs/russian/` + a line in both indexes, in the repository README (EN + RU) and in the modules table.
- Community files: [🛡️ SECURITY](SECURITY.md) and [🤝 CODE_OF_CONDUCT](CODE_OF_CONDUCT.md) — at the repository root, where GitHub's community-profile checks expect them (Russian versions in `docs/russian/`); the current version is duplicated in [version.txt](version.txt) — keep it in sync with `Cargo.toml`.

## 🛠️ Repository specifics

- **Language**: Rust. Daemon + CLI (`asc`) + a separate updater utility (`asc-updater`, see [docs/english/updater.md](docs/english/updater.md)). **All sources live in `src/`** (`src/cli/`, `src/daemon/`, `src/updater/`).
- **Target OSes**: Debian and Ubuntu first; write code with future support for other distributions and macOS in mind — distro-specific bits only behind abstractions.
- **Contributing**: process, code and commit style — [CONTRIBUTING.md](CONTRIBUTING.md); CI — `.github/workflows/`.
- **License**: MIT with mandatory attribution (Omar El Sayed @statebyte, AdminService.Cloud, Anytecture Software) — never remove the copyright header from [LICENSE](LICENSE).
- **Proto contracts**: the `proto/` directory in this repository is the **source of truth** for the daemon API; the platform links the contracts from here (a buf dependency). Changing the API — change the `.proto` here, backward-compatibly.
- **Open source**: the daemon is fully autonomous without the platform (CLI + local API) — do not add features that require the platform for basic operation.
- **Localization**: user-facing CLI messages only through the translation system (`src/daemon/i18n/`, EN default + RU); the language is the `language` setting in config.toml, command `asc config lang`; debug messages are not translated.
- **Skills**: skills for AI agents live in `skills/<asc-name>/SKILL.md` following the Agent Skills standard; SKILL.md texts are in English; every skill describes a fallback for when `asc` is not installed (rules — [skills/README.md](skills/README.md)).
- **Platforms**: Linux (major distributions), macOS (Apple Silicon); x86_64, ARM64, ARMv7 — changes must not break cross-compilation.
- **Unix-only compilation**: there are no Windows stubs or `cfg(not(unix))` branches in the code and there must be none (`lib.rs` has a `compile_error!`). On a Windows development machine `.cargo/config.toml` targets cargo at `x86_64-unknown-linux-gnu` (check/clippy work locally), while `cargo build`/`cargo test` run in WSL (Ubuntu) or CI.
