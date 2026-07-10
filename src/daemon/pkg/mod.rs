//! Package manager (DMN-003): `asc.yaml` manifests, registries (apt-style
//! sources), install by cloning the package repository (versions = git tags).

pub mod auth;
pub mod install;
pub mod manifest;
pub mod registry;
pub mod sources;
pub mod upgrade;

pub use install::{InstallReport, install};
pub use registry::RegistryClient;
pub use sources::SourceList;
pub use upgrade::{UpgradeOutcome, upgrade};
