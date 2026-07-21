//! Per-user credentials for private package repositories and image
//! registries (DMN-003, DMN-045, DMN-046).
//!
//! One store holds both kinds, told apart by `type` ([`Kind`]): `repo`
//! credentials authorize `git clone`, `registry` credentials authorize the
//! Docker Engine image pull. Both are keyed by a host or host/prefix
//! (`github.com/myorg`, `ghcr.io/myorg`) and may additionally be bound to a
//! single application ([`Credential::app`], its DMN-044 uuid or id), so a
//! token can be scoped to exactly the app that needs it.
//!
//! The file is JSON, 0600, one per scope: `/etc/asc/auth.json` (root) and
//! `~/.asc/auth.json` (user, alongside the rest of the DMN-041 tree). The
//! pre-DMN-045 TOML files (`/etc/asc/git-auth.toml`,
//! `~/.config/asc/git-auth.toml`) are still read when no JSON store exists
//! and are migrated on the next write, so configured auth keeps working.
//!
//! A token is handed to git through `GIT_ASKPASS` plus a process environment
//! variable — never through the URL or argv, which any local user can read
//! via /proc. SSH keys go through `GIT_SSH_COMMAND` with `IdentitiesOnly`.
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

const DEFAULT_SYSTEM_PATH: &str = "/etc/asc/auth.json";
/// Pre-DMN-045 TOML store, read-only (migrated on the next write).
const LEGACY_SYSTEM_PATH: &str = "/etc/asc/git-auth.toml";

/// What a credential authorizes against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Git repository — used by `git clone`/`fetch`.
    #[default]
    Repo,
    /// Container image registry — used by the Docker Engine image pull.
    Registry,
}

impl Kind {
    pub fn label(&self) -> &'static str {
        match self {
            Kind::Repo => "repo",
            Kind::Registry => "registry",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "repo" | "git" => Ok(Kind::Repo),
            "registry" | "docker" => Ok(Kind::Registry),
            other => bail!("unknown credential type '{other}': use 'repo' or 'registry'"),
        }
    }
}

/// One credential: how to authorize against `pattern`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credential {
    /// `repo` (default, also for legacy entries without the field) or `registry`.
    #[serde(rename = "type", default)]
    pub kind: Kind,
    /// Normalized host or host/prefix, e.g. `github.com/myorg`, `ghcr.io`.
    pub pattern: String,
    /// Registry user name — the Engine needs it alongside the token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Bind to a single application (DMN-044 uuid, or its id). `None` — the
    /// credential applies to every app whose URL/image matches `pattern`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(flatten)]
    pub method: Method,
}

/// Untagged on purpose: the JSON stays flat and readable —
/// `"token": "..."` or `"key": "/path"` right next to `pattern`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Method {
    /// Access token for https URLs and registries (GitHub PAT and alike).
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

/// JSON store: `{"credentials": [...]}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AuthFile {
    #[serde(default)]
    credentials: Vec<Credential>,
}

/// Pre-DMN-045 TOML store: `[[credential]]` tables.
#[derive(Debug, Clone, Default, Deserialize)]
struct LegacyAuthFile {
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
    /// System file: `$ASC_GIT_AUTH` override or `/etc/asc/auth.json`.
    pub fn system_path() -> PathBuf {
        std::env::var_os("ASC_GIT_AUTH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_SYSTEM_PATH))
    }

    /// User file: `$ASC_USER_GIT_AUTH` override or `~/.asc/auth.json`.
    pub fn user_path() -> Result<PathBuf> {
        if let Some(path) = std::env::var_os("ASC_USER_GIT_AUTH") {
            return Ok(PathBuf::from(path));
        }
        let home = std::env::var_os("HOME").context("cannot determine home directory ($HOME)")?;
        Ok(PathBuf::from(home).join(".asc/auth.json"))
    }

    /// Pre-DMN-045 TOML paths, consulted only when the JSON store is absent.
    fn legacy_system_path() -> PathBuf {
        PathBuf::from(LEGACY_SYSTEM_PATH)
    }

    fn legacy_user_path() -> Option<PathBuf> {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/asc/git-auth.toml"))
    }

    pub fn load() -> Result<Self> {
        Self::load_with(Scope::current())
    }

    pub fn load_with(scope: Scope) -> Result<Self> {
        // An explicit path override also opts out of the legacy fallback:
        // whoever points ASC_*_GIT_AUTH at a file means that file exactly.
        let legacy_system = std::env::var_os("ASC_GIT_AUTH")
            .is_none()
            .then(Self::legacy_system_path);
        let (user, legacy_user) = match scope {
            Scope::System => (None, None),
            Scope::User => (
                Some(Self::user_path()?),
                std::env::var_os("ASC_USER_GIT_AUTH")
                    .is_none()
                    .then(Self::legacy_user_path)
                    .flatten(),
            ),
        };
        Self::load_paths(
            &Self::system_path(),
            legacy_system.as_deref(),
            user.as_deref(),
            legacy_user.as_deref(),
            scope,
        )
    }

    /// Path-explicit loader — the env-free core of [`Self::load_with`], so
    /// tests can exercise the legacy fallback without touching process env.
    /// A missing JSON store falls back to the legacy TOML one, so an install
    /// configured before DMN-045 keeps authenticating until the next write
    /// migrates it.
    fn load_paths(
        system_path: &Path,
        legacy_system: Option<&Path>,
        user_path: Option<&Path>,
        legacy_user: Option<&Path>,
        scope: Scope,
    ) -> Result<Self> {
        let read_with_fallback = |json: &Path, legacy: Option<&Path>| -> Result<Vec<Credential>> {
            if let Some(creds) = read_auth(json)? {
                return Ok(creds);
            }
            match legacy {
                Some(path) => Ok(read_legacy_auth(path)?.unwrap_or_default()),
                None => Ok(Vec::new()),
            }
        };
        let system = read_with_fallback(system_path, legacy_system)?;
        let user = match user_path {
            Some(path) => read_with_fallback(path, legacy_user)?,
            None => Vec::new(),
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
        let raw = serde_json::to_string_pretty(&AuthFile {
            credentials: credentials.clone(),
        })
        .context("cannot serialize credentials")?;
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
    /// An entry is identified by `(kind, pattern, app)`, so the same host can
    /// carry a repo and a registry credential, and an app-bound one next to
    /// a general one.
    pub fn add(
        &mut self,
        kind: Kind,
        target: &str,
        method: Method,
        username: Option<String>,
        app: Option<String>,
    ) -> Result<&Credential> {
        let pattern = normalize(target);
        if pattern.is_empty() {
            bail!("cannot derive a host from '{target}'");
        }
        let list = match self.scope {
            Scope::System => &mut self.system,
            Scope::User => &mut self.user,
        };
        list.retain(|c| !(c.kind == kind && c.pattern == pattern && c.app == app));
        list.push(Credential {
            kind,
            pattern,
            username,
            app,
            method,
        });
        Ok(list.last().expect("just pushed"))
    }

    /// Remove credentials for a host or prefix. `kind` narrows the removal to
    /// one type; `None` removes every entry with that pattern.
    pub fn remove(&mut self, kind: Option<Kind>, target: &str) -> Result<()> {
        let pattern = normalize(target);
        let list = match self.scope {
            Scope::System => &mut self.system,
            Scope::User => &mut self.user,
        };
        let before = list.len();
        list.retain(|c| c.pattern != pattern || kind.is_some_and(|k| c.kind != k));
        if list.len() == before {
            bail!("no credentials for '{pattern}'");
        }
        Ok(())
    }

    /// Best credential for a git repository URL (`Kind::Repo`).
    pub fn lookup(&self, git_url: &str) -> Option<&Credential> {
        self.lookup_for(Kind::Repo, &normalize(git_url), None)
    }

    /// Best credential for a container image reference (`Kind::Registry`),
    /// e.g. `ghcr.io/org/app:1.0` or the implicit-Docker-Hub `nginx:1.28`.
    ///
    /// `apps` are the identities the app answers to — its id and, once it
    /// exists, its DMN-044 uuid — so `--app` accepts either spelling.
    pub fn lookup_registry(&self, image: &str, apps: &[&str]) -> Option<&Credential> {
        let target = normalize_image(image);
        // Prefer a credential bound to one of this app's identities; fall
        // back to an unbound one.
        apps.iter()
            .find_map(|app| self.lookup_for(Kind::Registry, &target, Some(app)))
            .or_else(|| self.lookup_for(Kind::Registry, &target, None))
    }

    /// Longest matching prefix of the right kind. `list()` puts user entries
    /// first and only a strictly longer pattern replaces the candidate, so
    /// user entries win ties over system ones.
    ///
    /// A credential bound to an app (`app: Some`) only ever matches that app,
    /// and beats an equally specific unbound one — binding is a deliberate
    /// narrowing, so it should win where it applies.
    fn lookup_for(&self, kind: Kind, target: &str, app: Option<&str>) -> Option<&Credential> {
        let mut best: Option<&Credential> = None;
        for (cred, _) in self.list() {
            if cred.kind != kind || !prefix_matches(&cred.pattern, target) {
                continue;
            }
            match &cred.app {
                // Bound to a different app (or to an app while we have none).
                Some(bound) if app != Some(bound.as_str()) => continue,
                _ => {}
            }
            let better = match best {
                None => true,
                Some(b) => match cred.pattern.len().cmp(&b.pattern.len()) {
                    std::cmp::Ordering::Greater => true,
                    std::cmp::Ordering::Equal => cred.app.is_some() && b.app.is_none(),
                    std::cmp::Ordering::Less => false,
                },
            };
            if better {
                best = Some(cred);
            }
        }
        best
    }
}

fn read_auth(path: &Path) -> Result<Option<Vec<Credential>>> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            let file: AuthFile = serde_json::from_str(&raw)
                .with_context(|| format!("invalid credentials file {}", path.display()))?;
            Ok(Some(file.credentials))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            Err(e).with_context(|| format!("cannot read credentials file {}", path.display()))
        }
    }
}

/// Read a pre-DMN-045 TOML store. Entries have no `type` and are therefore
/// all `Kind::Repo` — the only kind that existed.
fn read_legacy_auth(path: &Path) -> Result<Option<Vec<Credential>>> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            let file: LegacyAuthFile = toml::from_str(&raw)
                .with_context(|| format!("invalid credentials file {}", path.display()))?;
            Ok(Some(file.credentials))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        // A legacy file we cannot read must not break the whole command:
        // the JSON store is the source of truth now.
        Err(_) => Ok(None),
    }
}

/// Normalize a container image reference to `registry/repository` form, so
/// registry credentials match with the same prefix logic as repositories.
/// A reference without a registry belongs to Docker Hub, and the first
/// component counts as a registry only when it looks like a host.
pub fn normalize_image(image: &str) -> String {
    let image = image.trim();
    // Strip the tag/digest: the part after the last ':' is a tag only when it
    // contains no '/', otherwise the colon belonged to a registry port.
    let repo = match image.rsplit_once('@') {
        Some((head, _digest)) => head,
        None => match image.rsplit_once(':') {
            Some((head, tail)) if !tail.contains('/') => head,
            _ => image,
        },
    };
    match repo.split_once('/') {
        Some((head, _)) if head.contains('.') || head.contains(':') || head == "localhost" => {
            repo.to_string()
        }
        _ => format!("docker.io/{repo}"),
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

    /// A repo credential with no app binding — the common case.
    fn repo(pattern: &str, token: &str) -> Credential {
        Credential {
            kind: Kind::Repo,
            pattern: pattern.into(),
            username: None,
            app: None,
            method: Method::Token {
                token: token.into(),
            },
        }
    }

    #[test]
    fn lookup_prefers_longest_prefix_at_boundaries() {
        let auth = GitAuth {
            system: vec![repo("github.com", "sys")],
            user: vec![repo("github.com/org", "usr")],
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
        let auth = GitAuth {
            system: vec![repo("github.com", "sys")],
            user: vec![repo("github.com", "usr")],
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
        let path = dir.path().join("auth.json");
        unsafe { std::env::set_var("ASC_USER_GIT_AUTH", &path) };
        // System file must not leak in from the host machine.
        unsafe { std::env::set_var("ASC_GIT_AUTH", dir.path().join("none.json")) };

        let mut auth = GitAuth::load_with(Scope::User).unwrap();
        auth.add(
            Kind::Repo,
            "https://github.com/org/repo.git",
            Method::Token {
                token: "secret".into(),
            },
            None,
            None,
        )
        .unwrap();
        auth.add(
            Kind::Repo,
            "gitlab.com",
            Method::SshKey {
                key: PathBuf::from("/home/u/.ssh/id_ed25519"),
            },
            None,
            None,
        )
        .unwrap();
        // Same host, different type: both must survive.
        auth.add(
            Kind::Registry,
            "ghcr.io/org",
            Method::Token {
                token: "ghp".into(),
            },
            Some("statebyte".into()),
            None,
        )
        .unwrap();
        auth.save().unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "credentials file must be 0600");

        let auth = GitAuth::load_with(Scope::User).unwrap();
        assert_eq!(auth.list().len(), 3);
        assert!(auth.lookup("git@gitlab.com:a/b.git").is_some());
        // A registry entry must not answer a git lookup, and vice versa.
        assert!(auth.lookup("https://ghcr.io/org/repo").is_none());
        let hit = auth.lookup_registry("ghcr.io/org/app:1.0", &[]).unwrap();
        assert_eq!(hit.username.as_deref(), Some("statebyte"));

        let mut auth = auth;
        auth.remove(None, "github.com/org/repo").unwrap();
        assert!(auth.remove(None, "github.com/org/repo").is_err());

        unsafe { std::env::remove_var("ASC_USER_GIT_AUTH") };
        unsafe { std::env::remove_var("ASC_GIT_AUTH") };
    }

    #[test]
    fn image_reference_normalization() {
        assert_eq!(normalize_image("ghcr.io/org/app:1.0"), "ghcr.io/org/app");
        assert_eq!(normalize_image("nginx:1.28"), "docker.io/nginx");
        assert_eq!(normalize_image("library/nginx"), "docker.io/library/nginx");
        // A colon before a '/' is a registry port, not a tag.
        assert_eq!(normalize_image("localhost:5000/app"), "localhost:5000/app");
        assert_eq!(
            normalize_image("ghcr.io/org/app@sha256:abc"),
            "ghcr.io/org/app"
        );
    }

    #[test]
    fn app_bound_credential_only_serves_its_app() {
        let bound = Credential {
            app: Some("6f8a-uuid".into()),
            ..repo("github.com/org", "bound")
        };
        let auth = GitAuth {
            system: Vec::new(),
            user: vec![repo("github.com/org", "general"), bound],
            scope: Scope::User,
        };
        // The bound entry wins for its own app...
        let hit = auth
            .lookup_for(Kind::Repo, "github.com/org/repo", Some("6f8a-uuid"))
            .unwrap();
        assert!(matches!(&hit.method, Method::Token { token } if token == "bound"));
        // ...and is invisible to every other app.
        let hit = auth
            .lookup_for(Kind::Repo, "github.com/org/repo", Some("other"))
            .unwrap();
        assert!(matches!(&hit.method, Method::Token { token } if token == "general"));
        let hit = auth
            .lookup_for(Kind::Repo, "github.com/org/repo", None)
            .unwrap();
        assert!(matches!(&hit.method, Method::Token { token } if token == "general"));
    }

    #[test]
    fn legacy_toml_store_is_read_when_no_json_exists() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("git-auth.toml");
        fs::write(
            &legacy,
            "[[credential]]\npattern = \"github.com/legacy\"\ntoken = \"old\"\n",
        )
        .unwrap();

        let auth = GitAuth::load_paths(
            &dir.path().join("no-system.json"),
            None,
            Some(&dir.path().join("no-user.json")),
            Some(&legacy),
            Scope::User,
        )
        .unwrap();

        let hit = auth.lookup("https://github.com/legacy/repo").unwrap();
        // No `type` in the legacy format — it must default to repo.
        assert_eq!(hit.kind, Kind::Repo);
        assert!(matches!(&hit.method, Method::Token { token } if token == "old"));
    }

    #[test]
    fn json_store_wins_over_the_legacy_file() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("git-auth.toml");
        let json = dir.path().join("auth.json");
        fs::write(
            &legacy,
            "[[credential]]\npattern = \"github.com\"\ntoken = \"old\"\n",
        )
        .unwrap();
        fs::write(
            &json,
            r#"{"credentials":[{"type":"repo","pattern":"github.com","token":"new"}]}"#,
        )
        .unwrap();

        let auth = GitAuth::load_paths(
            &dir.path().join("no-system.json"),
            None,
            Some(&json),
            Some(&legacy),
            Scope::User,
        )
        .unwrap();

        let hit = auth.lookup("https://github.com/org/repo").unwrap();
        assert!(matches!(&hit.method, Method::Token { token } if token == "new"));
        assert_eq!(auth.list().len(), 1, "legacy entries must not be merged in");
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
