//! Remote git refs: list a repository's tags and branches, and pick the
//! latest version, without cloning (DMN-047, DMN-048).
//!
//! The package version is a property of the **repository** (its git tags),
//! not of the registry index — so `asc install pkg` resolves the newest tag
//! by asking the repository directly, and `asc install pkg@` offers the full
//! list of tags and branches to choose from. Credentials come from the same
//! [`super::auth`] store as `git clone`.

use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::auth::{self, GitAuth};
use crate::daemon::i18n::{Msg, t};

/// Tags and branches a remote repository advertises.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteRefs {
    /// Tags, newest version first (semver-descending, see [`sort_tags`]).
    pub tags: Vec<String>,
    /// Branch names, in the order the remote listed them.
    pub branches: Vec<String>,
}

impl RemoteRefs {
    pub fn is_empty(&self) -> bool {
        self.tags.is_empty() && self.branches.is_empty()
    }

    /// The newest tag, or `None` when the repository has none.
    pub fn latest_tag(&self) -> Option<&str> {
        self.tags.first().map(String::as_str)
    }
}

/// List a repository's tags and branches with `git ls-remote` — no clone, one
/// cheap round-trip. Uses the configured credential for private repositories
/// and, like the clone path, never hangs on an interactive prompt.
pub fn ls_remote(git_url: &str) -> Result<RemoteRefs> {
    // Credentials are optional: a public repository lists fine without them,
    // and an unreadable auth file must not block the query.
    let auth = GitAuth::load().ok();
    let credential = auth.as_ref().and_then(|a| a.lookup(git_url));

    let mut cmd = Command::new("git");
    cmd.args(["ls-remote", "--tags", "--heads", git_url]);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let _askpass = auth::configure_git(&mut cmd, credential.map(|c| &c.method))?;

    let output = match cmd.output() {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(t(Msg::ErrGitNotFound)),
        Err(e) => return Err(e).context("cannot run git ls-remote"),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if credential.is_none() && auth::is_auth_failure(&stderr) {
            return Err(anyhow::Error::new(auth::AuthRequired {
                url: git_url.to_string(),
            }));
        }
        bail!("git ls-remote failed: {}", stderr.trim());
    }
    Ok(parse_ls_remote(&String::from_utf8_lossy(&output.stdout)))
}

/// Parse `git ls-remote --tags --heads` output into sorted refs.
///
/// Each line is `<sha>\t<ref>`. Annotated tags also appear as a peeled
/// `refs/tags/<tag>^{}` line pointing at the commit; both carry the same tag
/// name, so the `^{}` form is dropped to avoid duplicates.
fn parse_ls_remote(stdout: &str) -> RemoteRefs {
    let mut tags = Vec::new();
    let mut branches = Vec::new();
    for line in stdout.lines() {
        let Some((_sha, refname)) = line.split_once('\t') else {
            continue;
        };
        if let Some(tag) = refname.strip_prefix("refs/tags/") {
            let tag = tag.strip_suffix("^{}").unwrap_or(tag);
            if !tags.iter().any(|t| t == tag) {
                tags.push(tag.to_string());
            }
        } else if let Some(branch) = refname.strip_prefix("refs/heads/") {
            branches.push(branch.to_string());
        }
    }
    sort_tags(&mut tags);
    RemoteRefs { tags, branches }
}

/// Sort tags newest-first. Version-like tags (`v1.2.3`, `1.2`) are ordered by
/// their numeric components; a release outranks its own pre-release
/// (`1.2.0` > `1.2.0-rc1`). Anything non-numeric sorts to the end, keeping a
/// stable relative order so the list stays readable.
pub fn sort_tags(tags: &mut [String]) {
    tags.sort_by_key(|tag| std::cmp::Reverse(version_key(tag)));
}

/// A comparison key for a tag: `(numeric components, has-no-prerelease)`.
/// A `None` for the numeric part sorts a non-version tag last.
fn version_key(tag: &str) -> (Option<Vec<u64>>, bool) {
    let core = tag.strip_prefix('v').unwrap_or(tag);
    // Split off a pre-release / build suffix (`-rc1`, `+meta`).
    let (numeric, no_prerelease) = match core.split_once(['-', '+']) {
        Some((head, _)) => (head, false),
        None => (core, true),
    };
    let parts: Option<Vec<u64>> = numeric
        .split('.')
        .map(|p| p.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()
        .filter(|v| !v.is_empty());
    (parts, no_prerelease)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tags_and_branches_dropping_peeled_duplicates() {
        let out = "\
abc123\trefs/heads/main
def456\trefs/heads/develop
1111111\trefs/tags/v1.0.0
2222222\trefs/tags/v1.0.0^{}
3333333\trefs/tags/v1.2.0
4444444\trefs/tags/v1.2.0^{}
";
        let refs = parse_ls_remote(out);
        assert_eq!(refs.branches, vec!["main", "develop"]);
        // Peeled `^{}` entries must not double the tags.
        assert_eq!(refs.tags, vec!["v1.2.0", "v1.0.0"]);
        assert_eq!(refs.latest_tag(), Some("v1.2.0"));
    }

    #[test]
    fn sorts_versions_newest_first_with_prerelease_below_release() {
        let mut tags = vec![
            "v1.2.0".to_string(),
            "1.10.0".to_string(),
            "v1.2.0-rc1".to_string(),
            "v1.9.0".to_string(),
            "nightly".to_string(),
        ];
        sort_tags(&mut tags);
        assert_eq!(
            tags,
            vec!["1.10.0", "v1.9.0", "v1.2.0", "v1.2.0-rc1", "nightly"]
        );
    }

    #[test]
    fn latest_tag_is_none_without_tags() {
        let refs = RemoteRefs {
            tags: Vec::new(),
            branches: vec!["main".into()],
        };
        assert_eq!(refs.latest_tag(), None);
        assert!(!refs.is_empty());
    }
}
