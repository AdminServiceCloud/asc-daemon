# 📦 Add ASC support to a repository

> 🌍 **Language:** English · [🇷🇺 Russian version](../russian/repository-support.md)

## 📌 Description

An ASC package repository declares one application with `asc.yaml`, optional user-facing configuration with `asc.settings.yaml`, or a multi-application package with `asc.stack.yaml`.

## 🎯 Scenarios

### One Docker application

Create `asc.yaml` in the repository root:

```yaml
name: example-web
version: 1.0.0
type: docker
title: Example web application
description: A small ASC package
settings: ./asc.settings.yaml
runtime:
  image: ghcr.io/acme/example-web:1.0.0
healthcheck:
  http: /health
```

Add `asc.settings.yaml` for values the operator may change. Environment variables, published ports and volumes belong here, not in `asc.yaml`.

```yaml
quota: { max_cpu: 1, max_ram: 512M, max_disk: 2G }
settings:
  - key: http_port
    type: ports
    default: [8080]
    container: 3000
    limits: { min: 1024, max: 65535 }
    env: PORT
  - key: data
    type: volumes
    default: [/app/data]
  - key: admin_password
    type: secret
    required: true
    env: ADMIN_PASSWORD
```

### Several applications in one repository

Put `asc.stack.yaml` at the repository root and put each application manifest in the path it declares:

```yaml
name: example-stack
version: 1.0.0
apps:
  - name: database
    path: ./database
  - name: web
    path: ./web
    depends_on: [database]
  - name: metrics
    path: ./metrics
    optional: true
```

`asc install example-stack` installs non-optional applications in dependency order. `asc install example-stack/metrics` installs the selected optional application and its dependencies.

## 🏗️ Technical design

- The root must contain exactly one package entry point: `asc.yaml` for one app or `asc.stack.yaml` for a stack. Nested manifests are discovered only through the stack.
- `name`, `version` and `type` are required in `asc.yaml`. Docker packages require `runtime.image` or `runtime.image-build`; native packages require `runtime.start`.
- Supported setting types include `string`, `number`, `boolean`, `enum`, `secret`, `ports` and `volumes`. Settings with `env` are passed to the application.
- A `ports` setting selects the host port. `container` fixes the container-side port; `protocol` is `tcp`, `udp` or `both`.
- `depends_on` references names from the same stack. Unknown dependencies and cycles are rejected.

Validate against the published [manifest schemas](https://github.com/AdminServiceCloud/registry/tree/main/schema) before publishing.

## 🔗 Related tasks

DMN-003, DMN-017, DMN-030, DMN-052, DMN-057 in [ROADMAP.md](../../../asc-platform/ROADMAP.md).
