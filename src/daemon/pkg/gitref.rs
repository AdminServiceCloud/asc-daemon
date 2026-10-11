//! Remote git refs: list a repository's tags and branches, and pick the
//! latest version, without cloning.
//!
//! The package version is a property of the **repository** (its git tags),
//! not of the registry index — so `asc install pkg` resolves the newest tag
//! by asking the repository directly, and `asc install pkg@` offers the full
//! list of tags and branches to choose from. Credentials come from the same
//! [`super::auth`] store as `git clone`.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::auth::{self, GitAuth};
use crate::daemon::apps::UserContext;
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
/// cheap round-trip. Uses the credential `ctx` configured for private
/// repositories and, like the clone path, never hangs on an interactive
/// prompt.
pub fn ls_remote(git_url: &str, ctx: &UserContext) -> Result<RemoteRefs> {
    let stdout = ls_remote_raw(git_url, ctx, &["--tags", "--heads"], &[])?;
    Ok(parse_ls_remote(&stdout))
}

/// The commit `refname` (`HEAD`, `refs/heads/main`) points at on the remote,
/// without fetching anything; `None` when the remote has no such ref.
pub fn remote_commit(git_url: &str, ctx: &UserContext, refname: &str) -> Result<Option<String>> {
    let stdout = ls_remote_raw(git_url, ctx, &[], &[refname])?;
    Ok(stdout
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .find(|(_, name)| *name == refname)
        .map(|(sha, _)| sha.to_string()))
}

/// `git ls-remote <flags> <url> <patterns>`, with the credential and the
/// typed auth errors every remote query shares.
fn ls_remote_raw(
    git_url: &str,
    ctx: &UserContext,
    flags: &[&str],
    patterns: &[&str],
) -> Result<String> {
    // Credentials are optional: a public repository lists fine without them,
    // and an unreadable auth file must not block the query.
    let auth = GitAuth::load_for(ctx).ok();
    let credential = auth.as_ref().and_then(|a| a.lookup(git_url));
    // Same transport rule as the clone: the URL follows the credential, so
    // an SSH key is not silently ignored on an https URL.
    let url = auth::transport_url(git_url, credential.map(|c| &c.method));

    let mut cmd = Command::new("git");
    cmd.arg("ls-remote").args(flags).arg(&url).args(patterns);
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
        if let Some(credential) = credential
            && auth::is_auth_failure(&stderr)
        {
            return Err(anyhow::Error::new(auth::AuthRejected {
                url: git_url.to_string(),
                method: credential.method.label(),
                detail: stderr.to_string(),
            }));
        }
        if let Some(hint) = auth::host_key_hint(&stderr, &url) {
            bail!("git ls-remote failed: {} — {hint}", stderr.trim());
        }
        bail!("git ls-remote failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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

/// The first 7 characters of a commit sha — what git itself abbreviates to,
/// and what the CLI and the docs show.
pub fn short_commit(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// The commit the repository in `dir` is checked out at, `None` when it
/// cannot be read (no git, not a repository). Used both to report an
/// installed app's current commit (DMN-0XX) and, during an upgrade, to tell
/// "the branch moved" from "nothing changed" for apps that follow a branch
/// rather than a tag.
pub fn head_commit(dir: &Path) -> Option<String> {
    let output = local_git(dir)
        .args(["rev-parse", "HEAD"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// `git -C <repo>` for a command on an installed app's repository. The
/// daemon runs as root over a tree a regular user may own, which git refuses
/// as "dubious ownership" — `safe.directory` (honoured on the command line)
/// vouches for exactly this one repository. `LC_ALL=C` keeps the output the
/// parsers below read in one language.
fn local_git(repo: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo)
        .arg("-c")
        .arg(format!("safe.directory={}", repo.display()))
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    cmd
}

/// Run a local git command to completion; its stdout on success, git's own
/// message as the error otherwise.
fn local_git_output(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = match local_git(repo).args(args).output() {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(t(Msg::ErrGitNotFound)),
        Err(e) => return Err(e).context("cannot run git"),
    };
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// What an in-place fetch asks the remote for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchRef {
    /// The head of a branch.
    Branch(String),
    /// A tag, tried as given and then with a `v` prefix (`1.2.0`,
    /// `v1.2.0`) — the same two spellings a clone tries.
    Tag(String),
    /// The remote's default branch.
    Head,
}

impl FetchRef {
    /// The refspecs to try, in order.
    fn candidates(&self) -> Vec<(String, String)> {
        match self {
            Self::Branch(branch) => vec![(branch.clone(), format!("refs/heads/{branch}"))],
            Self::Tag(tag) => {
                let mut tags = vec![tag.clone()];
                if !tag.starts_with('v') {
                    tags.push(format!("v{tag}"));
                }
                tags.into_iter()
                    .map(|t| (t.clone(), format!("refs/tags/{t}")))
                    .collect()
            }
            Self::Head => vec![("HEAD".to_string(), "HEAD".to_string())],
        }
    }
}

/// `git fetch --depth 1 --no-tags <url> <ref>` into an installed app's
/// repository — the in-place update of a compose app, whose project (and
/// the data its relative bind mounts keep) lives inside that repository and
/// so cannot be swapped for a fresh clone. Nothing in the working tree
/// changes; the fetched commit only lands in the object store. Returns the
/// ref name that matched (the tag spelling that existed) and the commit.
pub(super) fn fetch_ref(
    repo: &Path,
    remote: &super::install::GitRemote,
    target: &FetchRef,
    report: Option<&dyn crate::daemon::progress::InstallReporter>,
) -> Result<(String, String)> {
    let repo_arg = repo.to_string_lossy();
    let safe = format!("safe.directory={}", repo.display());
    let mut last_err = None;
    for (name, refspec) in target.candidates() {
        let args = [
            "-C",
            &repo_arg,
            "-c",
            &safe,
            "fetch",
            "--depth",
            "1",
            "--no-tags",
            "--progress",
            &remote.transport_url,
            &refspec,
        ];
        match remote.run(&args, "fetch", report) {
            Ok(()) => {
                let commit = local_git_output(repo, &["rev-parse", "FETCH_HEAD^{commit}"])?;
                let commit = String::from_utf8_lossy(&commit).trim().to_string();
                return Ok((name, commit));
            }
            // Missing access and a cancelled caller fail every spelling
            // alike: no point in trying the next one.
            Err(err)
                if err.downcast_ref::<auth::AuthRequired>().is_some()
                    || err.downcast_ref::<auth::AuthRejected>().is_some()
                    || err
                        .downcast_ref::<crate::daemon::progress::Cancelled>()
                        .is_some() =>
            {
                return Err(err);
            }
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("nothing to fetch")))
}

/// Tracked files whose working-tree (or staged) content differs from
/// `HEAD` — edited, deleted or added by hand.
pub(super) fn tracked_changes(repo: &Path) -> Result<Vec<String>> {
    let out = local_git_output(repo, &["diff", "--name-only", "-z", "HEAD"])?;
    Ok(out
        .split(|b| *b == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect())
}

/// The local edits of tracked files as a patch (`git diff --binary HEAD`),
/// for [`apply_patch`] to put back.
pub(super) fn tracked_changes_patch(repo: &Path) -> Result<Vec<u8>> {
    local_git_output(repo, &["diff", "--binary", "HEAD"])
}

/// Files `git` does not track here (ignored ones included) that a checkout
/// of `commit` would overwrite because `commit` does track them.
pub(super) fn untracked_in_the_way(repo: &Path, commit: &str) -> Result<Vec<String>> {
    let untracked = local_git_output(repo, &["ls-files", "--others", "-z"])?;
    let incoming = local_git_output(repo, &["ls-tree", "-r", "--name-only", "-z", commit])?;
    let incoming: std::collections::HashSet<&[u8]> = incoming
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .collect();
    Ok(untracked
        .split(|b| *b == 0)
        .filter(|path| !path.is_empty() && incoming.contains(path))
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect())
}

/// `git checkout --force --detach <commit>`: tracked files become exactly
/// `commit`'s, untracked and ignored ones are left alone (unless `commit`
/// tracks the same path — see [`untracked_in_the_way`]).
pub(super) fn checkout_detached(repo: &Path, commit: &str) -> Result<()> {
    local_git_output(
        repo,
        &["checkout", "--quiet", "--force", "--detach", commit],
    )
    .map(|_| ())
}

/// Put a patch from [`tracked_changes_patch`] back onto the working tree.
pub(super) fn apply_patch(repo: &Path, patch: &Path) -> Result<()> {
    let patch = patch.to_string_lossy();
    local_git_output(repo, &["apply", "--whitespace=nowarn", &patch]).map(|_| ())
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
    fn fetch_refs_name_exactly_one_kind_of_ref() {
        assert_eq!(
            FetchRef::Branch("main".into()).candidates(),
            [("main".to_string(), "refs/heads/main".to_string())]
        );
        // A tag is tried as given, then with the `v` a clone also tries.
        assert_eq!(
            FetchRef::Tag("1.2.0".into()).candidates(),
            [
                ("1.2.0".to_string(), "refs/tags/1.2.0".to_string()),
                ("v1.2.0".to_string(), "refs/tags/v1.2.0".to_string())
            ]
        );
        assert_eq!(
            FetchRef::Tag("v1.2.0".into()).candidates(),
            [("v1.2.0".to_string(), "refs/tags/v1.2.0".to_string())]
        );
        assert_eq!(
            FetchRef::Head.candidates(),
            [("HEAD".to_string(), "HEAD".to_string())]
        );
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn a_fetch_lands_the_ref_without_touching_the_working_tree() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("skipping: git is not available");
            return;
        }
        let upstream = tempfile::tempdir().unwrap();
        git(upstream.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(upstream.path().join("a.txt"), "one\n").unwrap();
        git(upstream.path(), &["add", "."]);
        git(upstream.path(), &["commit", "-q", "-m", "one"]);
        git(upstream.path(), &["tag", "v1.0.0"]);

        let work = tempfile::tempdir().unwrap();
        let clone = work.path().join("repository");
        git(
            work.path(),
            &[
                "clone",
                "-q",
                &upstream.path().to_string_lossy(),
                &clone.to_string_lossy(),
            ],
        );
        let installed = head_commit(&clone).unwrap();

        std::fs::write(upstream.path().join("a.txt"), "two\n").unwrap();
        git(upstream.path(), &["commit", "-q", "-am", "two"]);
        let upstream_head = head_commit(upstream.path()).unwrap();

        let ctx = UserContext {
            uid: 0,
            name: "root".into(),
            is_root: true,
        };
        let remote =
            super::super::install::GitRemote::resolve(&upstream.path().to_string_lossy(), &ctx);
        let (name, commit) =
            fetch_ref(&clone, &remote, &FetchRef::Branch("main".into()), None).unwrap();
        assert_eq!(name, "main");
        assert_eq!(commit, upstream_head);
        // Fetched, not checked out.
        assert_eq!(head_commit(&clone).unwrap(), installed);
        assert_eq!(
            std::fs::read_to_string(clone.join("a.txt")).unwrap(),
            "one\n"
        );

        // `1.0.0` is found under its `v` spelling.
        let (name, commit) =
            fetch_ref(&clone, &remote, &FetchRef::Tag("1.0.0".into()), None).unwrap();
        assert_eq!(name, "v1.0.0");
        assert_eq!(commit, installed);
        assert!(fetch_ref(&clone, &remote, &FetchRef::Tag("9.9.9".into()), None).is_err());

        checkout_detached(&clone, &upstream_head).unwrap();
        assert_eq!(
            std::fs::read_to_string(clone.join("a.txt")).unwrap(),
            "two\n"
        );
        assert!(tracked_changes(&clone).unwrap().is_empty());
        std::fs::write(clone.join("a.txt"), "edited\n").unwrap();
        assert_eq!(tracked_changes(&clone).unwrap(), ["a.txt"]);
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
