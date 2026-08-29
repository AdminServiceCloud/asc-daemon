# 🔄 asc-updater — the update management utility

## 📌 Description

A separate utility (a separate `asc-updater` binary) for installing and updating the daemon. It is deliberately split out of the daemon: it can update and restart the daemon itself without depending on it, and it keeps working even if the daemon is broken. It manages automatic updates — they can be enabled, configured or turned off entirely.

## 🎯 Scenarios

- 📥 **Initial installation**: `install.sh` installs `asc-updater`, which downloads and installs the daemon.
- ⚙️ **Interactive installation**: at install time the utility shows the default settings and asks the user — install with them or change:

```
⚙️  ASC installation settings (defaults):

  Language:                en
  Auto-updates:            enabled
  Update channel:          stable
  Check schedule:          daily 04:00
  Install directory:       /usr/local/bin

Install with the default settings? [Y/n/change]
```

- 🤫 **Silent installation**: `install.sh --silent` (or `asc-updater install --silent`) — one command without a single question: all defaults, dependencies installed automatically. For CI, scripts and AI agents; everything can be configured after installation.
- 🔄 **Auto-updates**: checking for new releases on a schedule, updating with respect to the daemon's active tasks.
- 🚫 **Disabling**: `asc-updater auto disable` — updates become manual-only.
- 🆘 **Rollback**: `asc-updater rollback` — return to the previous version if something goes wrong.
- 🤖 **Platform automation**: `status --json` reports version state as JSON and `update --json` streams machine-readable progress without localized text mixed into stdout.

## 🏗️ Technical design

### CLI

```bash
asc-updater install [--silent|--interactive]    # install the daemon (interactive by default; --silent — no questions, all defaults)
asc-updater update [--force] [--json]           # update now; --json streams NDJSON progress
asc-updater auto enable|disable|status          # manage auto-updates
asc-updater channel stable|beta                 # the update channel
asc-updater rollback                            # roll back to the previous version
asc-updater status [--json]                     # versions: installed / available
```

An alias from the daemon: `asc autoupdate enable|disable` proxies to `asc-updater auto ...`.

### Behavior

- **Root check**: the installer (`install.sh` and `asc-updater install`) first checks it runs as root/sudo — otherwise it stops with a clear message (`Please run as root: sudo …`). Without root it is impossible to install dependencies, create the systemd unit and the `/asc` directory.
- **Schedule**: a systemd timer (independent of the daemon's health); the check time is configurable.
- **Coordination with the daemon**: before updating, the utility asks the daemon about active tasks (installation, backup) — if there are any, the update is postponed; `--force` skips the wait.
- **Security**: releases are downloaded from GitHub Releases; signature/checksum verification is mandatory; the previous binary is kept for rollback.
- **Version policy**: installed and available versions are parsed as SemVer after an optional leading `v` is removed. The binary is replaced only when the available version is strictly newer; equal, older and invalid versions never trigger an update.
- **Configuration**: the `[updater]` section in `/etc/asc/config.toml` (enabled, channel, schedule); the settings chosen at install time are saved there too.
- **Updating the utility itself**: the daemon can update `asc-updater` (mutual updating — neither component is an unkillable single point of failure).

### Machine-readable output

`asc-updater status --json` writes one JSON object. `updateAvailable` is `true` only for a valid, strictly newer SemVer release. A missing/invalid installed version or a failed release lookup is an error with a non-zero exit status, so automation cannot mistake a failed check for an up-to-date daemon.

```json
{"updaterVersion":"0.17.0","installedVersion":"0.16.0","availableVersion":"v0.17.0","channel":"stable","updateAvailable":true}
```

`asc-updater update --json` writes one JSON object per line (NDJSON) and flushes each event immediately. Human/localized output is suppressed in this mode. Event fields are `type`, `stage`, optional `percent`, optional `version`, and a safe `message`. Stable stage codes are `checking`, `downloading`, `verifying_checksum`, `backing_up`, `replacing`, `restarting`, `completed`, and `error`.

```jsonl
{"type":"progress","stage":"checking","percent":0,"message":"Checking available daemon version"}
{"type":"progress","stage":"downloading","percent":15,"version":"v0.17.0","message":"Downloading release archive"}
{"type":"complete","stage":"completed","percent":100,"version":"v0.17.0","message":"Daemon update completed"}
```

Failures write an event with `type: "error"` and `stage: "error"`, then exit with a non-zero status. `percent` and `version` are omitted from error events.

## 🔗 Related tasks

DMN-001, DMN-014, DMN-064 in [ROADMAP.md](../../../asc-platform/ROADMAP.md).
