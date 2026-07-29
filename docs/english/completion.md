# ⌨️ Shell completion (Tab)

> 🌍 **Language:** English · [🇷🇺 Русская версия](../russian/completion.md)

## 📌 Description

Tab completion for `asc` in **bash**, **zsh** and **fish**: commands and subcommands, flags, enum values (`en|ru`, `on|off`, `cpu|mem`) and — the part a generated table cannot give — **live values**: installed apps, packages from the registry cache, backup storages, registry sources and saved credentials. Arguments that take a path hand the work back to the shell, so `asc backup storage add s3 --key /et<Tab>` expands to `/etc/` exactly like `ls /et<Tab>`.

The installer places the scripts system-wide; `asc completion <shell>` prints them for a manual install.

## 🎯 Scenarios

- 👤 A user types `asc app st<Tab>` → `start` / `stop` (plus their one-line help in zsh and fish).
- 🚀 `asc app start <Tab>` → the ids of the apps this user may manage — the daemon answers, so a regular user sees their own apps and root sees everyone's.
- 📦 `asc install nex<Tab>` → package names from the registry index cache, with no network request and no cold-cache stall.
- 💾 `asc backup create myapp --storage <Tab>` → the configured storages (`local`, an S3 bucket, an SFTP host).
- 📁 `asc backup storage add s3 --key ~/.ssh/i<Tab>` → the shell's own path completion.
- 🧑‍💻 A daemon that is stopped, unreachable, or simply not installed costs a few candidates, never an error and never a frozen Tab.

## 🏗️ Technical design

### 🧩 Two commands

| Command | Who calls it | What it does |
|---|---|---|
| `asc completion bash\|zsh\|fish` | a human, the installer | prints the completion script for that shell |
| `asc __complete -- <words...>` | the script, on every Tab | prints the candidates for the last word |

The scripts hold **no command list of their own** (the cobra model used by kubectl, gh and docker, rather than a generated command table): they forward the tokenized command line to `asc __complete` and print what comes back. A new command, a new flag or a newly installed app is therefore completable immediately — nothing is regenerated and no file is reinstalled. Both commands live in [src/cli/complete.rs](../../src/cli/complete.rs); the scripts are in [completions/](../../completions/) and embedded into the binary at build time.

### 📡 Wire format

`asc __complete` prints one candidate per line, then an optional directive:

```text
start<TAB>Start the app and attach to its console
stop
:file
```

- `value<TAB>description` — zsh and fish render the description in a second column; bash uses the value alone.
- `:file` / `:dir` — "these are paths": the shell completes them itself (`compopt -o default`, `_files`, `__fish_complete_path`). This is what keeps path completion identical to the shell's own, including the trailing `/` on directories.

Candidates are filtered against the typed prefix **in asc**, so every shell gets the same answer.

### 🌳 Where candidates come from

The engine walks the live clap command tree, so it always matches the binary it ships with: subcommands and long flags of the level the cursor is in, `ValueEnum` variants for enum arguments, and paths for arguments carrying a `ValueHint`. On top of that, arguments are mapped to **live lists** by their position in the tree and their name:

| Argument | Candidates | Source |
|---|---|---|
| `id`, `app` (`asc app stop <id>`, `asc backup create <app>`, `asc auth add --app`) | installed apps | the daemon (`GET /v1/apps`), else the local app store |
| `spec` of `asc install`, `query` of `asc search` | packages | the registry **index cache** — never a network fetch |
| `spec` of `asc upgrade` | installed apps | as above |
| `--source`, `asc source remove <name>` | registry sources | `sources.toml` |
| `--storage`, `asc backup storage remove <name>` | backup storages | the storage list |
| `asc auth remove <target>` | saved credentials | the credential store |

Adding a command that names an app as `id` or `app`, or takes a `--storage`, needs no change to the completion code.

### 🛡️ Rules the engine obeys

Completion runs inside the user's shell on a keystroke, which fixes its constraints:

- **Never fails.** A missing daemon, a socket the user may not open, an unreadable `config.toml` — every one of them yields fewer candidates. `asc __complete` is dispatched *before* the config is loaded and without a tracing subscriber, so a broken config cannot break the Tab key and no log line can land in the prompt.
- **Never blocks.** Every dynamic lookup runs under a 400 ms budget; a hung daemon costs one blank Tab, not a frozen terminal.
- **Never goes to the network.** Registry candidates come from the on-disk cache only. `asc update` remains the command that refreshes it.

### 📦 Installation

`asc-updater install` and `asc-updater update` write the scripts by running the freshly installed binary (`asc completion <shell>`), so the script always matches the installed `asc`. Only directories that already exist are written to — their presence is what says the shell is installed on the host:

| Shell | File |
|---|---|
| bash | `/usr/share/bash-completion/completions/asc`, else `/etc/bash_completion.d/asc` |
| zsh | `/usr/share/zsh/vendor-completions/_asc`, else a `site-functions` directory |
| fish | `/usr/share/fish/vendor_completions.d/asc.fish` |

The whole step is best-effort: a host without zsh, a read-only `/usr` or an `asc` too old to know the command never fails an install or an update. Manually, for the current user:

```bash
asc completion bash | sudo tee /usr/share/bash-completion/completions/asc   # bash
asc completion zsh  | sudo tee /usr/share/zsh/vendor-completions/_asc       # zsh
asc completion fish | sudo tee /usr/share/fish/vendor_completions.d/asc.fish # fish
```

A new shell picks them up; bash-completion loads its file lazily, on the first `asc<Tab>`.

## 🔗 Related tasks

DMN-055 in [ROADMAP.md](../../../asc-platform/ROADMAP.md).
