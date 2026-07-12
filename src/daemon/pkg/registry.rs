//! Registry client: fetches registry indexes (registry.json → category files,
//! recursively through `children`), caches them with a TTL and resolves
//! package names across all configured sources (first source wins).
//!
//! Formats mirror `registry/schema/*.schema.json`. Unknown fields are kept
//! permissive here — the registry evolves independently of installed daemons.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::Digest;
use tracing::{debug, warn};

use super::sources::{Source, SourceList};
use crate::daemon::config::Config;
use crate::daemon::http;
use crate::daemon::i18n::{Msg, tf};

/// How long a cached index stays fresh; `asc update` bypasses it.
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Deserialize)]
pub struct RegistryIndex {
    pub name: String,
    #[serde(default)]
    pub categories: Vec<IndexRef>,
}

/// Reference to a category (or child subcategory) index file.
#[derive(Debug, Clone, Deserialize)]
pub struct IndexRef {
    pub name: String,
    pub index: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CategoryFile {
    pub category: String,
    #[serde(default)]
    pub children: Vec<IndexRef>,
    #[serde(default)]
    pub packages: Vec<PackageEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PackageEntry {
    pub name: String,
    /// `app` (asc.yaml) or `stack` (asc.stack.yaml).
    #[serde(rename = "type")]
    pub package_type: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Latest version — a git tag of the package repository.
    #[serde(default)]
    pub latest: Option<String>,
    pub source: PackageSource,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PackageSource {
    /// Repository to clone.
    pub git: String,
    /// Subdirectory with the manifest (monorepo packages).
    #[serde(default)]
    pub path: Option<String>,
}

/// A package resolved to the source it came from.
#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub source_name: String,
    pub entry: PackageEntry,
}

/// Result of refreshing one source during `asc update`.
#[derive(Debug, Clone)]
pub struct SourceUpdate {
    pub source_name: String,
    /// Packages indexed across all categories of the source.
    pub packages: usize,
}

pub struct RegistryClient {
    sources: SourceList,
    cache_dir: PathBuf,
}

/// Index cache location: root shares the daemon's data dir, regular users
/// cache under `~/.cache/asc` (they cannot write to /var/lib/asc).
fn cache_dir_for(config: &Config) -> PathBuf {
    // SAFETY: geteuid() has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        return config.daemon.data_dir.join("registry-cache");
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("asc/registry-cache")
}

impl RegistryClient {
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            sources: SourceList::load()?,
            cache_dir: cache_dir_for(config),
        })
    }

    /// Resolve a package by name; the first source that has it wins.
    pub fn resolve(&self, name: &str) -> Result<ResolvedPackage> {
        for (source, _) in self.sources.list() {
            match self.packages_of(source, false) {
                Ok(packages) => {
                    if let Some(entry) = packages.into_iter().find(|p| p.name == name) {
                        if entry.package_type != "app" {
                            bail!(tf(Msg::PkgStacksNotSupported, name));
                        }
                        return Ok(ResolvedPackage {
                            source_name: source.name.clone(),
                            entry,
                        });
                    }
                }
                Err(err) => {
                    warn!(source = %source.name, error = %format!("{err:#}"), "cannot read registry source");
                }
            }
        }
        bail!(tf(Msg::PkgNotFound, name))
    }

    /// Case-insensitive substring search over name/title/description/tags.
    /// Name conflicts between sources are resolved by source priority.
    pub fn search(&self, query: &str) -> Result<Vec<ResolvedPackage>> {
        let query = query.to_lowercase();
        let mut found: Vec<ResolvedPackage> = Vec::new();
        for (source, _) in self.sources.list() {
            let packages = match self.packages_of(source, false) {
                Ok(packages) => packages,
                Err(err) => {
                    warn!(source = %source.name, error = %format!("{err:#}"), "cannot read registry source");
                    continue;
                }
            };
            for entry in packages {
                let haystack = format!(
                    "{} {} {} {}",
                    entry.name,
                    entry.title.as_deref().unwrap_or(""),
                    entry.description.as_deref().unwrap_or(""),
                    entry.tags.join(" ")
                )
                .to_lowercase();
                if haystack.contains(&query) && !found.iter().any(|p| p.entry.name == entry.name) {
                    found.push(ResolvedPackage {
                        source_name: source.name.clone(),
                        entry,
                    });
                }
            }
        }
        found.sort_by(|a, b| a.entry.name.cmp(&b.entry.name));
        Ok(found)
    }

    /// Force-refresh all indexes of all sources (`asc update`).
    /// Returns per-source stats so the CLI can report what was indexed.
    pub fn update(&self) -> Result<Vec<SourceUpdate>> {
        let mut updated = Vec::new();
        for (source, _) in self.sources.list() {
            let packages = self
                .packages_of(source, true)
                .with_context(|| format!("cannot update source '{}'", source.name))?;
            updated.push(SourceUpdate {
                source_name: source.name.clone(),
                packages: packages.len(),
            });
        }
        Ok(updated)
    }

    /// All packages of a source, walking categories and children.
    fn packages_of(&self, source: &Source, force: bool) -> Result<Vec<PackageEntry>> {
        let raw = self.fetch(source, "registry.json", force)?;
        let index: RegistryIndex = serde_json::from_str(&raw)
            .with_context(|| format!("invalid registry.json from '{}'", source.name))?;
        let mut packages = Vec::new();
        let mut queue: Vec<String> = index.categories.into_iter().map(|c| c.index).collect();
        // Defensive bound: a miswired registry must not loop us forever.
        let mut budget = 1000;
        while let Some(rel) = queue.pop() {
            budget -= 1;
            if budget == 0 {
                bail!("registry '{}' has too many index files", source.name);
            }
            let raw = self.fetch(source, &rel, force)?;
            let category: CategoryFile = serde_json::from_str(&raw).with_context(|| {
                format!("invalid category index '{rel}' from '{}'", source.name)
            })?;
            debug!(source = %source.name, category = %category.category, "category loaded");
            packages.extend(category.packages);
            queue.extend(category.children.into_iter().map(|c| c.index));
        }
        Ok(packages)
    }

    /// Read a registry file through the cache.
    fn fetch(&self, source: &Source, rel: &str, force: bool) -> Result<String> {
        let cache_file = source_cache_dir(&self.cache_dir, source).join(rel.replace('/', "__"));
        if !force && let Ok(meta) = fs::metadata(&cache_file) {
            let fresh = meta
                .modified()
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok())
                .is_some_and(|age| age < CACHE_TTL);
            if fresh && let Ok(raw) = fs::read_to_string(&cache_file) {
                return Ok(raw);
            }
        }
        let raw = fetch_uncached(&source.url, rel)?;
        if let Some(dir) = cache_file.parent() {
            // Cache failures are not fatal — the data is already in hand.
            if let Err(err) = fs::create_dir_all(dir).and_then(|()| fs::write(&cache_file, &raw)) {
                warn!(file = %cache_file.display(), error = %err, "cannot write registry cache");
            }
        }
        Ok(raw)
    }
}

/// Cache subdirectory of one source: its name plus a URL fingerprint.
/// The name alone is not enough — a source re-added under the same name
/// with a different URL must not serve stale indexes from the old location
/// until the TTL runs out.
fn source_cache_dir(cache_dir: &Path, source: &Source) -> PathBuf {
    let digest = sha2::Sha256::digest(source.url.as_bytes());
    cache_dir.join(format!(
        "{}-{:02x}{:02x}{:02x}{:02x}",
        source.name, digest[0], digest[1], digest[2], digest[3]
    ))
}

fn fetch_uncached(base_url: &str, rel: &str) -> Result<String> {
    if let Some(path) = base_url.strip_prefix("file://") {
        let path = PathBuf::from(path).join(rel);
        return fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()));
    }
    let url = format!("{}/{}", base_url.trim_end_matches('/'), rel);
    http::get_string(&url)
}

/// Convenience: build a `file://` source URL from a local directory (tests, dev).
pub fn file_source_url(dir: &Path) -> String {
    format!("file://{}", dir.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_dir_depends_on_source_url() {
        let cache = Path::new("/cache");
        let source = |url: &str| Source {
            name: "local".into(),
            url: url.into(),
        };
        let a = source_cache_dir(cache, &source("file:///tmp/one"));
        let b = source_cache_dir(cache, &source("file:///tmp/two"));
        assert_ne!(a, b, "same name, different URL must not share a cache");
        assert!(a.starts_with("/cache"));
        assert!(
            a.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("local-")
        );
    }
}
