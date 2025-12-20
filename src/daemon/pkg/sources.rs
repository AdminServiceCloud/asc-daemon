//! Source lists — which registries the daemon installs from (apt-style).
//!
//! Two levels (DMN-003): the **system** list `/etc/asc/sources.toml`,
//! managed by root and visible to every user, and a **per-user** list
//! `~/.config/asc/sources.toml` that supplements it. The effective list is
//! system sources first (higher priority on name conflicts), then the
//! user's own; users cannot shadow or remove system sources.
//!
//! Stored separately from config.toml (like `sources.list`): the config is
//! daemon settings, sources are content subscriptions. Order matters: on a
//! package name conflict the first (highest-priority) source wins.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::daemon::i18n::{Msg, tf};

/// Official registry, present unless explicitly removed (from the system list).
pub const OFFICIAL_NAME: &str = "official";
pub const OFFICIAL_URL: &str = "https://raw.githubusercontent.com/AdminServiceCloud/registry/main";

const DEFAULT_SOURCES_PATH: &str = "/etc/asc/sources.toml";

/// Which list a source lives in (and which list edits go to).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `/etc/asc/sources.toml` — root-managed, visible to all users.
    System,
    /// `~/.config/asc/sources.toml` — the calling user's own additions.
    User,
}

impl Scope {
    /// Editing scope of the current process: root manages the system list,
    /// everyone else their own.
    pub fn current() -> Scope {
        // SAFETY: geteuid() has no preconditions and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            Scope::System
        } else {
            Scope::User
        }
    }

    /// Technical label for tables and API output (not translated).
    pub fn label(self) -> &'static str {
        match self {
            Scope::System => "system",
            Scope::User => "user",
        }
    }
}

/// One registry source: `https://...` (registry root or GitHub raw) or
/// `file://...` (local directory with registry.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SourcesFile {
    #[serde(default, rename = "source")]
    sources: Vec<Source>,
}

/// System and user lists together; edits apply to `scope`.
#[derive(Debug, Clone)]
pub struct SourceList {
    system: Vec<Source>,
    user: Vec<Source>,
    scope: Scope,
}

impl SourceList {
    /// System sources file: `$ASC_SOURCES` override or the platform default.
    pub fn system_path() -> PathBuf {
        std::env::var_os("ASC_SOURCES")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_SOURCES_PATH))
    }

    /// The calling user's sources file: `$ASC_USER_SOURCES` override or
    /// `~/.config/asc/sources.toml`.
    pub fn user_path() -> Result<PathBuf> {
        if let Some(path) = std::env::var_os("ASC_USER_SOURCES") {
            return Ok(PathBuf::from(path));
        }
        let home = std::env::var_os("HOME").context("cannot determine home directory ($HOME)")?;
        Ok(PathBuf::from(home).join(".config/asc/sources.toml"))
    }

    /// Load both lists with the editing scope of the current user.
    pub fn load() -> Result<Self> {
        Self::load_with(Scope::current())
    }

    /// Load with an explicit editing scope (root edits the system list; the
    /// user list is still read so `list` shows the complete picture — except
    /// for root, whose "user" list is the system one).
    pub fn load_with(scope: Scope) -> Result<Self> {
        let system = match read_sources(&Self::system_path())? {
            Some(sources) => sources,
            // Missing system file means just the official registry.
            None => vec![Source {
                name: OFFICIAL_NAME.into(),
                url: OFFICIAL_URL.into(),
            }],
        };
        let user = match scope {
            Scope::System => Vec::new(),
            Scope::User => read_sources(&Self::user_path()?)?.unwrap_or_default(),
        };
        Ok(Self {
            system,
            user,
            scope,
        })
    }

    /// Persist the editable list (system for root, the user's own otherwise).
    pub fn save(&self) -> Result<()> {
        let (path, sources) = match self.scope {
            Scope::System => (Self::system_path(), &self.system),
            Scope::User => (Self::user_path()?, &self.user),
        };
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            fs::create_dir_all(dir)
                .with_context(|| format!("cannot create directory {}", dir.display()))?;
        }
        let raw = toml::to_string_pretty(&SourcesFile {
            sources: sources.clone(),
        })
        .context("cannot serialize sources")?;
        fs::write(&path, raw)
            .with_context(|| format!("cannot write sources file {}", path.display()))?;
        Ok(())
    }

    /// Effective sources in priority order: system first, then the user's
    /// own (minus any name that a system source already claims).
    pub fn list(&self) -> Vec<(&Source, Scope)> {
        let mut all: Vec<(&Source, Scope)> =
            self.system.iter().map(|s| (s, Scope::System)).collect();
        for source in &self.user {
            if !self.system.iter().any(|s| s.name == source.name) {
                all.push((source, Scope::User));
            }
        }
        all
    }

    /// Add a source to the editable list; the name defaults to the URL's
    /// host/last segment. Names must be unique across both lists — a user
    /// cannot shadow a system source.
    pub fn add(&mut self, url: &str, name: Option<&str>) -> Result<&Source> {
        if !url.starts_with("https://") && !url.starts_with("file://") {
            bail!("unsupported source url '{url}': use https:// or file://");
        }
        let name = match name {
            Some(name) => name.to_string(),
            None => derive_name(url),
        };
        if self.list().iter().any(|(s, _)| s.name == name) {
            bail!("source '{name}' already exists");
        }
        let target = match self.scope {
            Scope::System => &mut self.system,
            Scope::User => &mut self.user,
        };
        target.push(Source {
            name,
            url: url.trim_end_matches('/').to_string(),
        });
        Ok(target.last().expect("just pushed"))
    }

    /// Remove a source from the editable list. Pointing a regular user at a
    /// system source produces a dedicated error instead of "not found".
    pub fn remove(&mut self, name: &str) -> Result<()> {
        let target = match self.scope {
            Scope::System => &mut self.system,
            Scope::User => &mut self.user,
        };
        let before = target.len();
        target.retain(|s| s.name != name);
        if target.len() == before {
            if self.scope == Scope::User && self.system.iter().any(|s| s.name == name) {
                bail!(tf(Msg::SourceSystemNeedsRoot, name));
            }
            bail!("source '{name}' not found");
        }
        Ok(())
    }
}

/// Read a sources file; `None` when it does not exist.
fn read_sources(path: &Path) -> Result<Option<Vec<Source>>> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            let file: SourcesFile = toml::from_str(&raw)
                .with_context(|| format!("invalid sources file {}", path.display()))?;
            Ok(Some(file.sources))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read sources file {}", path.display())),
    }
}

/// A readable default name from a URL: host for https, directory for file.
fn derive_name(url: &str) -> String {
    let stripped = url
        .trim_start_matches("https://")
        .trim_start_matches("file://");
    let candidate = stripped
        .split(['/', '\\'])
        .rfind(|s| !s.is_empty())
        .unwrap_or("source");
    let name: String = candidate
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    if name.is_empty() {
        "source".into()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(system: &[(&str, &str)], user: &[(&str, &str)], scope: Scope) -> SourceList {
        let make = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(n, u)| Source {
                    name: n.to_string(),
                    url: u.to_string(),
                })
                .collect()
        };
        SourceList {
            system: make(system),
            user: make(user),
            scope,
        }
    }

    #[test]
    fn effective_list_prefers_system_and_appends_user() {
        let l = list(
            &[("official", "https://a"), ("corp", "https://b")],
            &[("corp", "https://evil"), ("mine", "https://c")],
            Scope::User,
        );
        let names: Vec<(&str, Scope)> = l
            .list()
            .iter()
            .map(|(s, scope)| (s.name.as_str(), *scope))
            .collect();
        // The user's "corp" clone is shadowed by the system source.
        assert_eq!(
            names,
            vec![
                ("official", Scope::System),
                ("corp", Scope::System),
                ("mine", Scope::User),
            ]
        );
    }

    #[test]
    fn user_cannot_shadow_or_remove_system_sources() {
        let mut l = list(&[("official", "https://a")], &[], Scope::User);
        assert!(l.add("https://x", Some("official")).is_err());
        let err = l.remove("official").unwrap_err().to_string();
        assert!(err.contains("official"));
        assert!(err.to_lowercase().contains("sudo"), "got: {err}");
    }

    #[test]
    fn user_manages_own_list() {
        let mut l = list(&[("official", "https://a")], &[], Scope::User);
        l.add("https://registry.example.com", None).unwrap();
        assert_eq!(l.list().len(), 2);
        l.remove("registry-example-com").unwrap();
        assert_eq!(l.list().len(), 1);
    }

    #[test]
    fn add_remove_and_conflicts() {
        let mut l = list(&[], &[], Scope::System);
        l.add("https://registry.example.com", None).unwrap();
        assert_eq!(l.list()[0].0.name, "registry-example-com");
        assert!(l.add("https://registry.example.com", None).is_err()); // same name
        assert!(l.add("ftp://nope", None).is_err());
        l.add("file:///tmp/reg", Some("local")).unwrap();
        l.remove("local").unwrap();
        assert!(l.remove("local").is_err());
    }

    #[test]
    fn defaults_to_official() {
        // Guard against a stray env var in the test environment.
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("ASC_SOURCES", dir.path().join("none.toml")) };
        unsafe { std::env::set_var("ASC_USER_SOURCES", dir.path().join("user.toml")) };
        let l = SourceList::load_with(Scope::User).unwrap();
        unsafe { std::env::remove_var("ASC_SOURCES") };
        unsafe { std::env::remove_var("ASC_USER_SOURCES") };
        assert_eq!(l.list().len(), 1);
        assert_eq!(l.list()[0].0.name, OFFICIAL_NAME);
        assert_eq!(l.list()[0].1, Scope::System);
    }
}
