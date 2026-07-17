//! Daemon modules. Future subsystems (tunnel, mcp, sftp, db) get their own
//! submodules here.

pub mod api;
pub mod apps;
pub mod backup;
pub mod client;
pub mod config;
pub mod console;
pub mod docker;
pub mod http;
pub mod i18n;
pub mod logging;
pub mod monitor;
pub mod pkg;
pub mod progress;
pub mod scheduler;
pub mod server;
pub mod service;
