//! Git authorization for private package repositories (DMN-003).
//!
//! Credentials are keyed by git host or host/prefix (`github.com`,
//! `github.com/myorg`) and live outside the source lists, in a 0600 file
//! per scope: `/etc/asc/git-auth.toml` (root) and
//! `~/.config/asc/git-auth.toml` (user). A token is handed to git through
//! `GIT_ASKPASS` plus a process environment variable — never through the
//! URL or argv, which any local user can read via /proc. SSH keys go
//! through `GIT_SSH_COMMAND` with `IdentitiesOnly`.
//!
//! Every git invocation disables interactive prompts, so cloning a private
//! repository without configured auth fails fast with a recognizable error
//! (see [`is_auth_failure`]) instead of hanging on a password prompt.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::sources::Scope;
use crate::daemon::i18n::{Msg, tf};

const DEFAULT_SYSTEM_PATH: &str = "/etc/asc/git-auth.toml";

/// One credential: how to authorize against repositories under `pattern`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credential {
    /// Normalized host or host/prefix, e.g. `github.com/myorg`.
    pub pattern: String,
    #[serde(flatten)]
    pub method: Method,
}

/// Untagged on purpose: the TOML stays flat and readable —
/// `token = "..."` or `key = "/path"` right next to `pattern`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Method {
    /// Access token for https URLs (GitHub PAT and alike).
    Token { token: String },
    /// Private key for git@/ssh URLs.
    SshKey { key: PathBuf },
}

impl Method {
    /// Method name for tables and messages — never the secret itself.
    pub fn label(&self) -> String {
        match self {
            Method::Token { .. } => "token".into(),
            Method::SshKey { key } => format!("ssh-key {}", key.display()),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AuthFile {
    #[serde(default, rename = "credential")]
    credentials: Vec<Credential>,
}

/// System and user credentials together; edits apply to `scope`
/// (mirrors [`super::sources::SourceList`]).
#[derive(Debug, Clone)]
pub struct GitAuth {
    system: Vec<Credential>,
    user: Vec<Credential>,
    scope: Scope,
}

impl GitAuth {
    /// System file: `$ASC_GIT_AUTH` override or `/etc/asc/git-auth.toml`.
    pub fn system_path() -> PathBuf {
        std::env::var_os("ASC_GIT_AUTH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_SYSTEM_PATH))
    }

    /// User file: `$ASC_USER_GIT_AUTH` override or `~/.config/asc/git-auth.toml`.
    pub fn user_path() -> Result<PathBuf> {
        if let Some(path) = std::env::var_os("ASC_USER_GIT_AUTH") {
            return Ok(PathBuf::from(path));
        }
        let home = std::env::var_os("HOME").context("cannot determine home directory ($HOME)")?;
        Ok(PathBuf::from(home).join(".config/asc/git-auth.toml"))
    }

    pub fn load() -> Result<Self> {
        Self::load_with(Scope::current())
    }

    pub fn load_with(scope: Scope) -> Result<Self> {
        let system = read_auth(&Self::system_path())?.unwrap_or_default();
        let user = match scope {
            Scope::System => Vec::new(),
            Scope::User => read_auth(&Self::user_path()?)?.unwrap_or_default(),
        };
        Ok(Self {
            system,
            user,
            scope,
        })
    }

    /// Persist the editable list with owner-only permissions.
    pub fn save(&self) -> Result<()> {
        let (path, credentials) = match self.scope {
            Scope::System => (Self::system_path(), &self.system),
            Scope::User => (Self::user_path()?, &self.user),
        };
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            fs::create_dir_all(dir)
                .with_context(|| format!("cannot create directory {}", dir.display()))?;
        }
        let raw = toml::to_string_pretty(&AuthFile {
            credentials: credentials.clone(),
        })
        .context("cannot serialize git credentials")?;
        fs::write(&path, raw)
            .with_context(|| format!("cannot write credentials file {}", path.display()))?;
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("cannot set permissions on {}", path.display()))?;
        }
        Ok(())
    }

    /// All credentials in lookup priority order: the user's own first.
    pub fn list(&self) -> Vec<(&Credential, Scope)> {
        let mut all: Vec<(&Credential, Scope)> =
            self.user.iter().map(|c| (c, Scope::User)).collect();
        all.extend(self.system.iter().map(|c| (c, Scope::System)));
        all
    }

    /// Add (or replace) a credential for a host/prefix in the editable list.
    pub fn add(&mut self, target: &str, method: Method) -> Result<&Credential> {
        let pattern = normalize(target);
        if pattern.is_empty() {
            bail!("cannot derive a git host from '{target}'");
        }
        let list = match self.scope {
            Scope::System => &mut self.system,
            Scope::User => &mut self.user,
        };
        list.retain(|c| c.pattern != pattern);
        list.push(Credential { pattern, method });
        Ok(list.last().expect("just pushed"))
    }

    /// Remove a credential from the editable list.
    pub fn remove(&mut self, target: &str) -> Result<()> {
        let pattern = normalize(target);
        let list = match self.scope {
            Scope::System => &mut self.system,
            Scope::User => &mut self.user,
        };
        let before = list.len();
        list.retain(|c| c.pattern != pattern);
        if list.len() == before {
            bail!("no credentials for '{pattern}'");
        }
        Ok(())
    }

    /// Best credential for a repository URL: the longest matching prefix.
    /// `list()` puts user entries first and only a strictly longer pattern
    /// replaces the candidate, so user entries win ties over system ones.
    pub fn lookup(&self, git_url: &str) -> Option<&Credential> {
        let url = normalize(git_url);
        let mut best: Option<&Credential> = None;
        for (cred, _) in self.list() {
            if prefix_matches(&cred.pattern, &url)
                && best.is_none_or(|b| cred.pattern.len() > b.pattern.len())
            {
                best = Some(cred);
            }
        }
        best
    }
}

fn read_auth(path: &Path) -> Result<Option<Vec<Credential>>> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            let file: AuthFile = toml::from_str(&raw)
                .with_context(|| format!("invalid credentials file {}", path.display()))?;
            Ok(Some(file.credentials))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            Err(e).with_context(|| format!("cannot read credentials file {}", path.display()))
        }
    }
}

/// Normalize a git URL (or a bare host/prefix) to `host/path` form:
/// `https://github.com/org/repo.git`, `git@github.com:org/repo.git` and
/// `ssh://git@github.com/org/repo` all become `github.com/org/repo`.
pub fn normalize(url: &str) -> String {
    let mut rest = url.trim();
    for scheme in ["https://", "http://", "ssh://", "file://"] {
        rest = rest.strip_prefix(scheme).unwrap_or(rest);
    }
    // scp-like syntax: git@host:path
    let rest = match rest.split_once('@') {
        Some((_user, tail)) => tail.replacen(':', "/", 1),
        None => rest.to_string(),
    };
    rest.trim_end_matches('/')
        .trim_end_matches(".git")
        .to_string()
}

/// `pattern` matches `url` at a path boundary: `github.com/org` matches
/// `github.com/org/repo` but not `github.com/organization`.
fn prefix_matches(pattern: &str, url: &str) -> bool {
    match url.strip_prefix(pattern) {
        Some(rest) => rest.is_empty() || rest.starts_with('/'),
        None => false,
    }
}

/// Whether the URL wants SSH auth (`git@...` / `ssh://`) or https tokens.
pub fn is_ssh_url(url: &str) -> bool {
    let url = url.trim();
    url.starts_with("ssh://") || (!url.contains("://") && url.contains('@') && url.contains(':'))
}

// ── Wiring credentials into a git process ────────────────────────────────────

/// Removes the temporary askpass helper when the git command is done.
pub struct GitEnvGuard {
    askpass: Option<PathBuf>,
}

impl Drop for GitEnvGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.askpass {
            let _ = fs::remove_file(path);
        }
    }
}

/// Configure a git `Command`: disable interactive prompts (fail fast on
/// private repositories) and inject the credential, if any. Keep the
/// returned guard alive until the command has run.
pub fn configure_git(cmd: &mut Command, method: Option<&Method>) -> Result<GitEnvGuard> {
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    match method {
        None => {
            cmd.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
            Ok(GitEnvGuard { askpass: None })
        }
        Some(Method::SshKey { key }) => {
            cmd.env(
                "GIT_SSH_COMMAND",
                format!(
                    "ssh -i '{}' -o IdentitiesOnly=yes -o BatchMode=yes",
                    key.display()
                ),
            );
            Ok(GitEnvGuard { askpass: None })
        }
        Some(Method::Token { token }) => {
            // The helper script is not secret (it echoes an env var); the
            // token lives only in this child's environment.
            let script = std::env::temp_dir().join(format!(
                "asc-askpass-{}-{:x}.sh",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            ));
            fs::write(
                &script,
                "#!/bin/sh\ncase \"$1\" in\n  *sername*) echo \"x-access-token\" ;;\n  *) printf '%s\\n' \"$ASC_GIT_TOKEN\" ;;\nesac\n",
            )
            .with_context(|| format!("cannot write askpass helper {}", script.display()))?;
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
                    .context("cannot mark askpass helper executable")?;
            }
            cmd.env("GIT_ASKPASS", &script)
                .env("ASC_GIT_TOKEN", token)
                .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
            Ok(GitEnvGuard {
                askpass: Some(script),
            })
        }
    }
}

/// Whether a failed git command's stderr looks like missing or rejected
/// authorization — i.e. the repository is (probably) private. GitHub answers
/// "Repository not found" for private repositories without access.
pub fn is_auth_failure(stderr: &str) -> bool {
    const MARKERS: &[&str] = &[
        "Authentication failed",
        "authentication failed",
        "could not read Username",
        "could not read Password",
        "Permission denied (publickey",
        "Repository not found",
        "terminal prompts disabled",
        "Invalid username or",
        "HTTP Basic: Access denied",
    ];
    MARKERS.iter().any(|m| stderr.contains(m))
}

/// Typed error: the repository looks private and no working credential is
/// configured. The CLI catches it to offer interactive setup; everyone else
/// sees the message with the `asc auth add` hint.
#[derive(Debug)]
pub struct AuthRequired {
    pub url: String,
}

impl fmt::Display for AuthRequired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", tf(Msg::PkgAuthRequired, &self.url))
    }
}

impl std::error::Error for AuthRequired {}

/// Private SSH keys under `~/.ssh` a user can pick from (files that are not
/// public keys or known config/metadata files).
pub fn list_ssh_keys(ssh_dir: &Path) -> Vec<PathBuf> {
    const SKIP: &[&str] = &[
        "known_hosts",
        "known_hosts.old",
        "config",
        "authorized_keys",
    ];
    let Ok(entries) = fs::read_dir(ssh_dir) else {
        return Vec::new();
    };
    let mut keys: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            !name.ends_with(".pub") && !SKIP.contains(&name) && !name.starts_with('.')
        })
        .filter(|p| {
            fs::read_to_string(p)
                .map(|c| c.contains("PRIVATE KEY"))
                .unwrap_or(false)
        })
        .collect();
    keys.sort();
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_normalization() {
        assert_eq!(
            normalize("https://github.com/org/repo.git"),
            "github.com/org/repo"
        );
        assert_eq!(
            normalize("git@github.com:org/repo.git"),
            "github.com/org/repo"
        );
        assert_eq!(
            normalize("ssh://git@github.com/org/repo"),
            "github.com/org/repo"
        );
        assert_eq!(normalize("github.com/org/"), "github.com/org");
        assert_eq!(normalize("github.com"), "github.com");
    }

    #[test]
    fn lookup_prefers_longest_prefix_at_boundaries() {
        let auth = GitAuth {
            system: vec![Credential {
                pattern: "github.com".into(),
                method: Method::Token {
                    token: "sys".into(),
                },
            }],
            user: vec![Credential {
                pattern: "github.com/org".into(),
                method: Method::Token {
                    token: "usr".into(),
                },
            }],
            scope: Scope::User,
        };
        let hit = auth.lookup("https://github.com/org/repo.git").unwrap();
        assert_eq!(hit.pattern, "github.com/org");
        // Boundary: "org" must not match "organization".
        let hit = auth.lookup("https://github.com/organization/x").unwrap();
        assert_eq!(hit.pattern, "github.com");
        assert!(auth.lookup("https://gitlab.com/x/y").is_none());
    }

    #[test]
    fn user_credential_wins_an_exact_tie_with_system() {
        let cred = |token: &str| Credential {
            pattern: "github.com".into(),
            method: Method::Token {
                token: token.into(),
            },
        };
        let auth = GitAuth {
            system: vec![cred("sys")],
            user: vec![cred("usr")],
            scope: Scope::User,
        };
        let hit = auth.lookup("https://github.com/org/repo").unwrap();
        assert!(matches!(&hit.method, Method::Token { token } if token == "usr"));
    }

    #[test]
    fn ssh_url_detection() {
        assert!(is_ssh_url("git@github.com:org/repo.git"));
        assert!(is_ssh_url("ssh://git@github.com/org/repo"));
        assert!(!is_ssh_url("https://github.com/org/repo"));
    }

    #[test]
    fn auth_failure_markers() {
        assert!(is_auth_failure(
            "fatal: could not read Username for 'https://github.com'"
        ));
        assert!(is_auth_failure("ERROR: Repository not found."));
        assert!(is_auth_failure(
            "git@github.com: Permission denied (publickey)."
        ));
        assert!(!is_auth_failure(
            "fatal: repository '/tmp/x' does not exist"
        ));
    }

    #[test]
    fn store_roundtrip_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("git-auth.toml");
        unsafe { std::env::set_var("ASC_USER_GIT_AUTH", &path) };
        // System file must not leak in from the host machine.
        unsafe { std::env::set_var("ASC_GIT_AUTH", dir.path().join("none.toml")) };

        let mut auth = GitAuth::load_with(Scope::User).unwrap();
        auth.add(
            "https://github.com/org/repo.git",
            Method::Token {
                token: "secret".into(),
            },
        )
        .unwrap();
        auth.add(
            "gitlab.com",
            Method::SshKey {
                key: PathBuf::from("/home/u/.ssh/id_ed25519"),
            },
        )
        .unwrap();
        auth.save().unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "credentials file must be 0600");

        let auth = GitAuth::load_with(Scope::User).unwrap();
        assert_eq!(auth.list().len(), 2);
        assert!(auth.lookup("git@gitlab.com:a/b.git").is_some());

        let mut auth = auth;
        auth.remove("github.com/org/repo").unwrap();
        assert!(auth.remove("github.com/org/repo").is_err());

        unsafe { std::env::remove_var("ASC_USER_GIT_AUTH") };
        unsafe { std::env::remove_var("ASC_GIT_AUTH") };
    }

    #[test]
    fn ssh_key_listing_skips_public_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let key = "-----BEGIN OPENSSH PRIVATE KEY-----\nx\n-----END OPENSSH PRIVATE KEY-----\n";
        fs::write(dir.path().join("id_ed25519"), key).unwrap();
        fs::write(dir.path().join("id_ed25519.pub"), "ssh-ed25519 AAA").unwrap();
        fs::write(dir.path().join("known_hosts"), "x").unwrap();
        fs::write(dir.path().join("config"), "Host *").unwrap();
        fs::write(dir.path().join("work_key"), key).unwrap();
        let keys = list_ssh_keys(dir.path());
        let names: Vec<_> = keys
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, vec!["id_ed25519", "work_key"]);
    }
}
