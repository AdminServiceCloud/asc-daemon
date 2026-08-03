# 🤖 The daemon's MCP server

## 📌 Description

`asc mcp serve` is a local [Model Context Protocol](https://modelcontextprotocol.io/)
server for AI clients. It uses standard input/output, while every management
operation goes to the running ASC daemon through its local Unix socket.

The daemon derives access from the kernel's `SO_PEERCRED` peer UID. There is
no user, UID, token or role parameter in the MCP protocol: a normal user can
only see and manage that user's applications, while a root MCP process can
manage every application. Run `sudo asc mcp serve` only when that full access
is intended.

## 🎯 Scenarios

- A developer connects Codex, Claude Code, or another stdio MCP client and
  manages applications they own without being added to the `docker` group.
- An administrator starts the same command as root to inspect or manage all
  applications on the host.
- An AI reads application state and logs, installs or upgrades applications,
  changes schema-validated settings, and creates or restores backups.

## 🏗️ Technical design

### Tools

| Tool | Action | MCP hint |
|---|---|---|
| `system_info`, `metrics_get` | daemon and application metrics | read-only |
| `app_list`, `app_info`, `logs_read`, `app_settings_get` | application data | read-only |
| `app_install`, `app_upgrade`, `app_control`, `app_settings_update` | application management | mutating |
| `backup_list`, `backup_create`, `backup_restore`, `backup_prune` | backups | restore is destructive |
| `app_remove`, `exec_command` | removal and host command execution | destructive |

App references are resolved by the daemon on every call. A foreign application
and an unknown application produce the same `not found` error, so ownership is
not disclosed. The server marks destructive tools with MCP annotations; an MCP
client should request confirmation according to its policy.

`exec_command` is deliberately executed by the MCP process, not by the root
daemon. It therefore inherits the real OS UID of `asc mcp serve`. Commands use
`/bin/sh -lc`, default to a 60-second timeout (maximum 300 seconds), and each
output stream is limited to 1 MiB.

### Connect an MCP client

Requirements:

- `asc` is installed and is on the client process `PATH`.
- The daemon is running and reachable: `asc status` must succeed.
- The client user can connect to the configured ASC Unix socket. The socket is
  deliberately world-connectable; the daemon still authorizes each request by
  its kernel-reported peer UID.

For a Codex-compatible stdio configuration:

```json
{
  "mcp_servers": {
    "asc": {
      "command": "asc",
      "args": ["mcp", "serve"]
    }
  }
}
```

Run the client normally to manage only its user's applications. For an
administrator session that intentionally needs all applications, configure the
command as `sudo` with arguments `asc`, `mcp`, `serve` (and arrange non-
interactive sudo according to the host's security policy).

For Claude Code:

```bash
claude mcp add asc -- asc mcp serve
```

If connection fails, first run `asc status`, then check the daemon service and
configured socket path. If `asc` is not found, install ASC or use its absolute
path in the client configuration. Do not grant root just to make a user's app
visible: verify that the app was installed under that user's UID instead.

## 🔗 Related tasks

DMN-013, AI-001, AI-002 and AI-003 in [ROADMAP.md](../../../asc-platform/ROADMAP.md).
