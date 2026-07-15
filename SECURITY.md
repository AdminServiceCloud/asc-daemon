# 🛡️ Security Policy

## 📦 Supported versions

Security fixes are released for the latest release of **asc-daemon**. Before `1.0.0`, only the most recent version receives fixes — always update to the latest release via `asc-updater`.

| Version | Supported |
|---|---|
| latest release | ✅ |
| older releases | ❌ (update via `asc-updater`) |

## 🚨 Reporting a vulnerability

The daemon runs with root privileges and manages user servers, so we take vulnerabilities seriously. If you find one, **please report it privately** — don't hesitate!

1. 🔒 Report it via [GitHub private vulnerability reporting](https://github.com/AdminServiceCloud/asc-daemon/security/advisories/new) or any of the private contact addresses listed in [Support](../../README.md#-support). **Do not open a public issue.**
2. 📝 Describe the vulnerability: affected component (daemon, CLI, updater, install.sh), steps to reproduce, impact. If you have a fix, that is most welcome — attach or summarize it in your message!
3. 🔍 We will evaluate the report and, if necessary, release a fix or mitigating steps. We will contact you with the outcome and credit you in the report.
4. 🤐 Please **do not disclose the vulnerability publicly** until a fix is released! Once we have either a) published a fix, or b) declined to address it for whatever reason, you are free to disclose it publicly.

## 🔐 Security practices

- 🔑 API access is token-based; tokens are stored with restrictive file permissions
- 📁 SFTP access is isolated per application
- 🧾 Secrets in environment variables are stored protected and never logged
- ⚠️ asc-daemon follows good security practices, but 100% security cannot be assured. The software is provided **"as is"** without any warranty — see [LICENSE](../../LICENSE)
