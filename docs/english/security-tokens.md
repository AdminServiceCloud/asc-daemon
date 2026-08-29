# 🔐 Security tokens: primary and access

[🇷🇺 Русская версия](../russian/security-tokens.md)

## 📌 Description

Two kinds of bearer token guard the daemon's TCP API.

The **primary token** is the one the daemon generates on first start and keeps in `api.token` next to config.toml (root-only, 0600). It never expires, it is the credential the platform stores at enrollment, and from DMN-065 on it behaves like a *refresh* token: its job is to mint other tokens and to be rotated, not to sign everyday traffic.

An **access token** is short-lived (10 minutes by default), lives only in the daemon's memory and is minted on demand by presenting the primary. It carries the same authority as the primary for everything except token management: it cannot read or rotate the primary, cannot mint further access tokens, and cannot revoke them. That is the whole difference.

Everything on the unix socket is unaffected — there identity comes from the kernel (`SO_PEERCRED`), not from a token. See [📡 api](api.md).

## 🎯 Scenarios

- The platform keeps the primary encrypted at rest and calls the node with a 10-minute access token, refreshing it through the primary when it expires or when the daemon restarts.
- An operator asks the platform for a temporary token, sees it once in the browser and uses it from a laptop: `curl -H "Authorization: Bearer <token>" https://node.example.com:8420/v1/apps`. Ten minutes later it stops working on its own.
- A token leaks. The operator presses "revoke all temporary tokens" — every outstanding access token dies immediately, and nothing else about the node changes.
- The node is compromised, or a support engagement ends. The operator rotates the primary from the danger zone; every access token is revoked as part of that, and the platform stores the new primary.
- Locally: `sudo asc api token show`, `sudo asc api token rotate`, `sudo asc api status`.

## 🏗️ Technical design

### Token store

One in-memory `TokenStore` behind `ApiState`:

- **Primary** — the current token plus, during a rotation, the previous one with a deadline (the grace window below).
- **Access** — a map keyed by the token's SHA-256 digest, not by the token itself: the map lives for minutes rather than the 30 seconds a console token does, and a memory dump or a stray `Debug` print must not yield working credentials.

Access tokens are multi-use until they expire (unlike single-use console tokens), swept lazily on every issue and every lookup, and capped at 64 live entries — past the cap the oldest is evicted, so a platform that leaks refreshes cannot grow the map without bound.

### Resolving a bearer

`auth()` classifies the presented token in this order: current primary → primary within the grace window → live access token → reject. A rejected request gets the same answers as before: REST `401`, gRPC `UNAUTHENTICATED`.

### What an access token may not do

Three endpoints require a primary (`require_primary`): rotate, commit a rotation, and mint or revoke access tokens. Everything else is open to it. On the unix socket the same guard accepts a **root** peer, which is what makes `sudo asc api token rotate` work while a regular user on the socket is refused.

> ⚠️ An access token is full machine control minus token management. It can install and start an application — that is, clone and run an arbitrary git repository as root — and it can open a console session inside a container. Treat handing one out as handing out a root shell with a ten-minute fuse.

### Revoking

`DELETE /v1/token/access` drops every live access token and answers with how many it dropped. The same routine runs as the first step of a rotation, so "revoke everything" exists in exactly one place. Revocation itself requires the primary: otherwise a leaked access token could kick the platform off the node. The consequence is that whoever holds a temporary token cannot revoke themselves — revocation goes through the platform, where it is audited.

### Rotating the primary

`POST /v1/token/rotate` mints a new primary, writes it **atomically** (temporary file → `0600` → `fsync` → `rename` → `fsync` on the directory, so an interrupted write can never leave an empty `api.token`) and returns it in plaintext. There is no other way for a platform that reaches the node directly to learn the new value.

Rotation is two-phase:

1. The daemon revokes all access tokens, installs the new primary and keeps the old one valid for a **grace window** (300 seconds by default, 3600 maximum, `0` to disable).
2. The caller persists the new token and confirms with `POST /v1/token/rotate/commit`, **using the new token**. A commit presented with the grace token is refused — otherwise a platform that stored nothing could report success.
3. If no commit arrives the window simply expires.

> ⚠️ The previous token is held in memory only, so restarting the daemon inside the grace window ends the window early. The grace is a cushion for a slow write on the other side, not a guarantee. If the new token is lost: the platform can re-read `api.token` over SSH on an SSH-transport node, or an operator can run `sudo asc api token show` on the machine. A node reachable only over direct TLS, with no SSH, has no automated recovery.

### Endpoints

| REST | gRPC | Guard | Description |
|---|---|---|---|
| `POST /v1/token/access` | `TokenService.IssueAccessToken` | primary | Mint a short-lived token; `{"ttl_secs"?, "label"?}` → `{"token", "expires_at", "ttl_secs"}` |
| `DELETE /v1/token/access` | `TokenService.RevokeAccessTokens` | primary | Kill every live access token → `{"revoked": n}` |
| `POST /v1/token/rotate` | `TokenService.RotatePrimaryToken` | primary | New primary; `{"grace_secs"?}` → `{"token", "rotated_at", "grace_until"}` |
| `POST /v1/token/rotate/commit` | `TokenService.CommitPrimaryTokenRotation` | primary, not the grace token | Confirm the rotation, end the window |
| `GET /v1/token` | `TokenService.GetTokenStatus` | any | Status, never a secret |

`GET /v1/token` answers with the caller's token kind, its expiry when it has one, the number of live access tokens, the rotation state and `primary_digest` — the first 16 hex characters of the primary's SHA-256. That is enough for the platform to answer "is the token I hold still the current one" without either side transmitting it.

### CLI

- `asc api status` — listener, TLS mode, domain, fingerprint, live access tokens, rotation state.
- `asc api token show` — print the primary (root, unix socket only).
- `asc api token rotate [--grace <secs>]`
- `asc api token issue [--ttl <secs>] [--label <text>]`
- `asc api token revoke` — revoke every access token.

## 🔗 Related tasks

- DMN-065 — the token model, endpoints and the denied set.
- DMN-066 — rotation: atomic write, grace window, two-phase commit, `asc api token`.
- DMN-061 — direct access to the daemon API over TLS.
- NODE-008 / NODE-009 — the platform side: minting, caching, rotation and revocation from the node's settings.
