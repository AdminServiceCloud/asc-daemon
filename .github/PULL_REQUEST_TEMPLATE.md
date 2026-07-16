<!--
Thanks for contributing to asc-daemon! Please fill out the sections below.
Keep the PR focused: one logical change per PR. See CONTRIBUTING.md.
-->

## 📋 Summary

<!-- What does this PR do and why? One or two sentences. -->

## 🔗 Related task / issue

<!-- Every change starts from a roadmap task (DMN-*) or an issue. Link it here. -->

- Roadmap task: DMN-
- Closes #

## 🧩 Type of change

<!-- Mark all that apply with an "x". -->

- [ ] ✨ Feature (`feat`)
- [ ] 🐛 Bug fix (`fix`)
- [ ] 📝 Documentation (`docs`)
- [ ] ♻️ Refactor (`refactor`)
- [ ] 🧪 Tests (`test`)
- [ ] 🔧 Chore / tooling (`chore`)

## 🗂️ Affected component

<!-- e.g. API (gRPC/REST), CLI, App management, Package manager, MCP server,
     Monitoring, Backups, SFTP, Consoles, Scheduler, Updater, Config -->

## 📝 What changed

<!-- Bullet the concrete changes so a reviewer can follow the diff. -->

-

## ✅ Checklist

- [ ] The change starts from a roadmap task, and the relevant doc in `docs/` is updated (docs first, then code — see `AGENTS.md`).
- [ ] `task check` passes locally (clippy with `-D warnings`, `cargo fmt`, tests).
- [ ] New logic has unit tests; bug fixes include a regression test.
- [ ] Commits follow Conventional Commits (`feat(pkg): ...`, `fix(cli): ...`).
- [ ] Does not break builds for x86_64 / ARM64 / ARMv7; distro-specific code sits behind an abstraction.
- [ ] User-facing CLI strings go through the translation system (EN + RU keys); debug/log messages are English-only.
- [ ] `proto/` changes are backward compatible (new fields optional, numbers not reused).

## 🔎 How to test

<!-- Commands / steps a reviewer can run to verify the change. Include real output where useful. -->

```bash

```

## 📌 Notes for reviewers

<!-- Anything else: trade-offs, follow-ups, screenshots, asciinema, breaking changes. -->
