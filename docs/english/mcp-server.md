# 🤖 The daemon's MCP server

## 📌 Description

An MCP server (Model Context Protocol) built into the daemon — a set of tools through which an AI manages the server. Used by the platform's AI assistant ([🤖 ai-assistant](../../../asc-platform/docs/features/ai-assistant.md)) as well as by any MCP client (Claude Desktop, Claude Code) when working locally — a strong move for the open source audience.

## 🎯 Scenarios

- The platform AI receives the daemon's tools through the nodeservice tunnel and executes the user's commands.
- A developer connects the daemon to Claude Code: `claude mcp add asc -- asc mcp serve` — and manages their server from the IDE.

## 🏗️ Technical design

### Tools (MVP)

| Tool | Action | Danger |
|---|---|---|
| `system_info` | OS, resources, uptime | 🟢 |
| `metrics_get` | System/application metrics | 🟢 |
| `app_list` / `app_info` | Application list and details | 🟢 |
| `logs_read` | Reading application logs | 🟢 |
| `app_install` / `app_control` | Install, start/stop/restart | 🟡 |
| `env_set` | Changing variables | 🟡 |
| `backup_create` / `backup_restore` | Backups | 🟡/🔴 |
| `app_remove` | Removing an application | 🔴 |
| `exec_command` | An arbitrary shell command | 🔴 |

- **Danger levels**: 🟢 execute immediately; 🟡 — by policy; 🔴 — always require user confirmation (elicitation / confirmation in the platform UI).
- **Transports**: stdio (local MCP clients) and streamable HTTP (through the platform tunnel).
- **Authorization**: the set of available tools is filtered by the permissions of the user the AI acts for ([🔐 access-control](../../../asc-platform/docs/features/access-control.md)).
- **Audit**: every tool call goes to the daemon log and the platform audit.

## 🔗 Related tasks

DMN-013, AI-001, AI-002, AI-003 in [ROADMAP.md](../../../asc-platform/ROADMAP.md).
