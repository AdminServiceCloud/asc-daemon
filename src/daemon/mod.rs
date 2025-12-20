//! Daemon modules. Future subsystems (api, tunnel, apps, pkg, mcp, backup,
//! monitor, sftp, db, console, scheduler) get their own submodules here.

pub mod api;
pub mod apps;
pub mod config;
pub mod console;
pub mod docker;
pub mod http;
pub mod i18n;
pub mod logging;
pub mod monitor;
pub mod pkg;
pub mod server;
pub mod service;
