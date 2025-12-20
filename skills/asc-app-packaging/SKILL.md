---
name: asc-app-packaging
description: Package an application for the ASC ecosystem - write asc.yaml (single app) or asc.stack.yaml (multi-app stack) manifests, validate them and publish to a registry. Use when the user wants to package, containerize or publish their app for AdminService.Cloud, create an asc.yaml/asc.stack.yaml, or add a package to an ASC registry.
---

# ASC App Packaging

Create manifests that make an app installable via `asc install <name>`.

## Root rule (important)

A repository may contain many `asc.yaml` files in subdirectories, but its **root must contain exactly one** manifest: either `asc.yaml` (single app) or `asc.stack.yaml` (stack connecting the nested apps). Nested manifests without a root one are not indexed.

## Single app: asc.yaml

```yaml
name: my-app                # kebab-case, unique in the registry
version: 1.2.0              # semver
type: docker                # docker | native | utility
category: web               # databases | ai | bots | game-servers | system-utilities | web
description: "Short English description"
runtime:
  image: nginx:1.27         # for docker; native/utility use install/start/stop commands
env:
  - name: PORT
    default: 8080
  - name: API_KEY
    secret: true
    required: true
ports: [8080]
volumes: [/data]
requirements: { ram: 256M, disk: 1G }
healthcheck: { http: /health }
```

## Multi-app stack: asc.stack.yaml

```yaml
name: my-stack
version: 1.0.0
description: "Web app with database"
apps:
  - name: web
    path: ./web             # directory containing its own asc.yaml
    depends_on: [db]
  - name: db
    path: ./db
env:                        # shared across all stack apps
  - name: TZ
    default: UTC
```

Install: `asc install my-stack` (whole stack) or `asc install my-stack/web` (one app).

## Workflow

1. Ask what the app is (docker image? native binary? multiple components?) and pick `asc.yaml` vs `asc.stack.yaml`.
2. Write the manifest(s). Descriptions in English for public registries.
3. Validate against JSON schemas from the official registry repo: `schema/asc.schema.json`, `schema/asc.stack.schema.json` (https://github.com/AdminServiceCloud/registry).
4. Test locally: `asc source add file:///path/to/repo && asc install <name>` (see asc-server-management skill if `asc` is missing).
5. Publish: PR to the official registry (add an entry to the matching `categories/<theme>.json`, English description, type `app` or `stack`) or just share the GitHub repo URL — users can add it with `asc source add`.

## Tips

- `database:` section auto-provisions a DB and injects credentials as env secrets — prefer it over bundling a DB manually for simple apps.
- Mark tokens/passwords with `secret: true` so the platform stores and masks them properly.
- Examples live in https://github.com/AdminServiceCloud/asc-example-apps.
