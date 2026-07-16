//! Package manager (DMN-003): `asc.yaml` manifests, registries (apt-style
//! sources), install by cloning the package repository (versions = git tags).

pub mod auth;
pub mod clone;
pub mod install;
pub mod manifest;
pub mod refresh;
pub mod registry;
pub mod settings;
pub mod sources;
pub mod upgrade;

pub use clone::clone_app;
pub use install::{
    AmbiguousPackage, GitRef, InstallOutcome, InstallReport, LicenseRequired, install,
    install_from_git, instance_id, is_git_url, repo_name,
};
pub(crate) use install::{VolumeKind, classify_volume, runtime_inputs};
pub use registry::RegistryClient;
pub use sources::SourceList;
pub use upgrade::{UpgradeOutcome, upgrade};
