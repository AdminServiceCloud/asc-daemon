# 🗂️ Create a custom registry

> 🌍 **Language:** English · [🇷🇺 Russian version](../russian/custom-registry.md)

## 📌 Description

An ASC registry is a static hierarchy of JSON files. The root `registry.json` links to category files, and category files list packages and optional child categories. The daemon can read it from GitHub raw content or any HTTPS site.

## 🎯 Scenarios

### Minimal registry

```text
my-registry/
├── registry.json
└── categories/
    └── web.json
```

```json
// registry.json
{
  "name": "acme-registry",
  "title": "Acme Registry",
  "format_version": 1,
  "categories": [{ "name": "web", "index": "categories/web.json" }]
}
```

```json
// categories/web.json
{
  "category": "web",
  "packages": [{
    "name": "example-web",
    "type": "app",
    "description": "Example web application",
    "source": { "git": "https://github.com/acme/example-web" }
  }]
}
```

### Publish from GitHub

Commit the files to a public repository and add the raw directory URL on the server:

```bash
asc source add https://raw.githubusercontent.com/acme/my-registry/main --name acme
asc update
asc search example-web
asc install example-web --source acme
```

### Publish from your own HTTPS site

Copy the directory to `/var/www/asc-registry` and serve it as static files. A minimal Nginx server block is:

```nginx
server {
  listen 443 ssl;
  server_name packages.example.com;
  root /var/www/asc-registry;
  location / { try_files $uri =404; }
}
```

Then connect the base URL, without `registry.json` at the end:

```bash
sudo asc source add https://packages.example.com --name acme
asc update
asc search example-web
```

## 🏗️ Technical design

- `registry.json` requires `name`, `format_version` and `categories`; category indexes require `category` and `packages`.
- Relative `index` paths are resolved from the registry root. `source.git` points to a package repository; `source.path` selects its manifest directory when needed.
- Package `type` is `app` for `asc.yaml` or `stack` for `asc.stack.yaml`.
- The registry must be publicly readable over HTTPS. Use [registry schemas](https://github.com/AdminServiceCloud/registry/tree/main/schema) to validate JSON before publishing.
- `sudo asc source add` creates a system source for every server user; without sudo it creates a source only for the current user.

## 🔗 Related tasks

DMN-003, REG-001, REG-003, DMN-057 in [ROADMAP.md](../../../asc-platform/ROADMAP.md).
