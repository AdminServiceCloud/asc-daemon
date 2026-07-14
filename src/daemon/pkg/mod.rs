//! Package manager (DMN-003): `asc.yaml` manifests, registries (apt-style
//! sources), install by cloning the package repository (versions = git tags).

pub mod auth;
pub mod install;
pub mod manifest;
pub mod refresh;
pub mod registry;
pub mod settings;
pub mod sources;
pub mod upgrade;

pub use install::{AmbiguousPackage, InstallOutcome, InstallReport, LicenseRequired, install};
pub use registry::RegistryClient;
pub use sources::SourceList;
pub use upgrade::{UpgradeOutcome, upgrade};
