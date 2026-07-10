# 🗄️ Database management (daemon)

## 📌 Description

Managing DBMSes on the node as first-class objects: creating databases, users and granting permissions — without manually poking around in psql/mysql. Supported: PostgreSQL, MySQL/MariaDB, MongoDB, Redis (installed as applications through ASC or already existing).

## 🎯 Scenarios

- 🆕 A user installs WordPress from the store — the platform automatically creates a MySQL database and user and injects the credentials into env.
- 👤 `asc db user add myapp_user --db myapp --grant rw` — creating a user from the CLI.
- 💾 The backup module takes a consistent dump through this module.

## 🏗️ Technical design

- **DBMS drivers**: the `DbDriver { create_db, drop_db, create_user, grant, list, dump, restore }` trait with implementations for PostgreSQL, MySQL, MongoDB, Redis (for Redis — ACL users).
- **Discovery**: DBMSes installed through ASC register automatically; external ones are connected manually (`asc db connect`).
- **DBMS admin credentials**: stored in the daemon's local storage (encrypted with the node key).
- **Auto-provisioning**: the `database:` section in `asc.yaml` — at application install time a database and user are created automatically and the credentials land in the application's env as secrets.
- **CLI**: `asc db list|create|drop`, `asc db user add|remove|list`, `asc db dump|restore`.

## 🔗 Related tasks

DMN-011, DMN-009 in [ROADMAP.md](../../../asc-platform/ROADMAP.md).
