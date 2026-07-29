//! Shell completion (DMN-055): the scripts `asc completion <shell>` prints and
//! the candidate engine `asc __complete` behind them.
//!
//! The split follows the cobra model (kubectl, gh, docker) rather than a
//! generated per-shell command table: the installed script is a thin,
//! version-independent delegate, and every candidate — subcommands, flags,
//! enum values, installed apps, registry packages, backup storages — is
//! computed here, in Rust, against the live clap tree and the live system. A
//! new command or a freshly installed app is therefore completable with no
//! regenerated script and no reinstalled file.
//!
//! Everything in this module obeys three rules, because it runs on a Tab press
//! inside the user's shell:
//!
//! 1. **Never fail.** No daemon, no permission on the socket, an unreadable
//!    config — all of it yields fewer candidates, never an error and never a
//!    line on stderr (the scripts redirect it away regardless).
//! 2. **Never block.** Anything that touches the daemon or the filesystem runs
//!    under [`DEADLINE`]; a hung daemon costs one blank Tab, not a frozen
//!    terminal.
//! 3. **Never go to the network.** Registry candidates come from the on-disk
//!    index cache only ([`RegistryClient::cached_packages`]).
//!
//! Wire format, one candidate per line:
//!
//! ```text
//! value<TAB>description
//! :file | :dir            (optional trailing directive)
//! ```
//!
//! The directive is how path completion stays the shell's job: asc says "these
//! are paths", and bash/zsh/fish complete them with their own machinery, so
//! `asc backup restore /et<Tab>` expands to `/etc/` exactly like `ls /et<Tab>`.

use std::sync::mpsc;
use std::time::Duration;

use clap::{Arg, ArgAction, Command as ClapCommand, CommandFactory, ValueHint};

use asc_daemon::daemon::apps::{AppManager, UserContext};
use asc_daemon::daemon::backup::storage::StorageList;
use asc_daemon::daemon::client;
use asc_daemon::daemon::config::Config;
use asc_daemon::daemon::pkg::RegistryClient;
use asc_daemon::daemon::pkg::auth::GitAuth;
use asc_daemon::daemon::pkg::sources::SourceList;

/// Time budget for one dynamic lookup (the daemon, the app store, the index
/// cache). A Tab press that takes longer than this reads as a hung shell, so
/// the candidates are dropped instead — the user simply types the name.
const DEADLINE: Duration = Duration::from_millis(400);

/// Longest description shipped next to a candidate; zsh and fish render these
/// in a column and a whole paragraph of clap help would wrap the screen.
const HELP_WIDTH: usize = 60;

/// Shells with a completion script (`asc completion <shell>`).
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

/// The script for one shell, embedded at build time from `completions/`.
pub fn script(shell: Shell) -> &'static str {
    match shell {
        Shell::Bash => include_str!("../../completions/asc.bash"),
        Shell::Zsh => include_str!("../../completions/asc.zsh"),
        Shell::Fish => include_str!("../../completions/asc.fish"),
    }
}

/// One completion candidate: what gets inserted, plus the one-liner shells
/// with description support (zsh, fish) show next to it.
pub struct Candidate {
    value: String,
    help: Option<String>,
}

impl Candidate {
    fn new(value: impl Into<String>, help: Option<&str>) -> Self {
        Self {
            value: value.into(),
            help: help.map(trim_help),
        }
    }
}

/// Whether the shell should fall back to completing paths itself, and which
/// kind — the `:file` / `:dir` directive of the wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paths {
    None,
    File,
    Dir,
}

/// What `asc __complete` answers with.
pub struct Reply {
    pub candidates: Vec<Candidate>,
    pub paths: Paths,
}

impl Reply {
    fn empty() -> Self {
        Self {
            candidates: Vec::new(),
            paths: Paths::None,
        }
    }

    fn values(candidates: Vec<Candidate>) -> Self {
        Self {
            candidates,
            paths: Paths::None,
        }
    }

    fn paths(paths: Paths) -> Self {
        Self {
            candidates: Vec::new(),
            paths,
        }
    }
}

/// `asc __complete -- <words...>`: print the candidates for the last word of
/// `words` (the one being typed, possibly empty). Infallible by contract — see
/// the module docs.
pub fn run(words: &[String], config: &Config) {
    let reply = complete(words, config);
    let mut out = String::new();
    for candidate in &reply.candidates {
        match &candidate.help {
            Some(help) => out.push_str(&format!("{}\t{help}\n", candidate.value)),
            None => out.push_str(&format!("{}\n", candidate.value)),
        }
    }
    match reply.paths {
        Paths::File => out.push_str(":file\n"),
        Paths::Dir => out.push_str(":dir\n"),
        Paths::None => {}
    }
    // `print!` panics on a closed pipe; a shell that walked away mid-completion
    // must not produce a Rust backtrace in the user's terminal.
    let _ = std::io::Write::write_all(&mut std::io::stdout(), out.as_bytes());
}

/// Candidates for the last word of the command line.
///
/// `words` is the line as the shell tokenized it: `words[0]` is the binary,
/// the last element is the word under the cursor (empty when the cursor sits
/// after a space), and everything between is already-typed arguments.
fn complete(words: &[String], config: &Config) -> Reply {
    let (current, typed) = match words.split_last() {
        Some((current, typed)) => (current.as_str(), typed),
        None => return Reply::empty(),
    };

    let mut cmd = crate::Cli::command();
    // Populates the implicit args (`--help`, `--version`) and normalizes the
    // tree, so what we walk is what clap itself would parse.
    cmd.build();

    // Walk the already-typed words, descending into subcommands as they
    // appear. What we need at the end: which command level the cursor is in,
    // how many of its positionals are already filled, and whether the previous
    // word was an option still waiting for its value.
    let mut path: Vec<String> = vec![cmd.get_name().to_string()];
    let mut filled_positionals = 0usize;
    let mut awaiting: Option<Arg> = None;
    let mut positionals_only = false;

    for word in typed.iter().skip(1) {
        if awaiting.take().is_some() {
            continue; // this word is the pending option's value
        }
        if positionals_only {
            filled_positionals += 1;
            continue;
        }
        if word == "--" {
            positionals_only = true;
            continue;
        }
        if let Some(long) = word.strip_prefix("--") {
            // `--name=value` carries its value inline: nothing pending.
            if !long.contains('=')
                && let Some(arg) = long_arg(&cmd, long)
                && takes_value(&arg)
            {
                awaiting = Some(arg);
            }
            continue;
        }
        if word.len() > 1
            && word.starts_with('-')
            && let Some(last) = word.chars().last()
        {
            // In a cluster (`-dn 50`) only the final short flag can take a value.
            if let Some(arg) = short_arg(&cmd, last)
                && takes_value(&arg)
            {
                awaiting = Some(arg);
            }
            continue;
        }
        match cmd.find_subcommand(word).cloned() {
            Some(sub) => {
                path.push(sub.get_name().to_string());
                cmd = sub;
                filled_positionals = 0;
            }
            None => filled_positionals += 1,
        }
    }

    // `--storage=loc<Tab>`: the value of an option written with `=`. Candidates
    // must carry the `--storage=` prefix — that whole string is the word the
    // shell replaces.
    if let Some(long) = current.strip_prefix("--")
        && let Some((name, typed_value)) = long.split_once('=')
        && let Some(arg) = long_arg(&cmd, name)
    {
        let prefix = format!("--{name}=");
        let mut reply = values_for(&path, &arg, config);
        for candidate in &mut reply.candidates {
            candidate.value = format!("{prefix}{}", candidate.value);
        }
        return filtered(reply, &format!("{prefix}{typed_value}"));
    }

    if let Some(arg) = awaiting {
        return filtered(values_for(&path, &arg, config), current);
    }

    if current.starts_with('-') {
        return filtered(Reply::values(flags(&cmd)), current);
    }

    // Neither a flag nor a flag's value: the subcommands of this level, plus
    // whatever its next unfilled positional accepts.
    let mut candidates: Vec<Candidate> = cmd
        .get_subcommands()
        .filter(|sub| !sub.is_hide_set())
        .map(|sub| {
            Candidate::new(
                sub.get_name(),
                sub.get_about().map(|a| a.to_string()).as_deref(),
            )
        })
        .collect();
    let mut paths = Paths::None;
    if let Some(arg) = cmd.get_positionals().nth(filled_positionals) {
        let reply = values_for(&path, arg, config);
        candidates.extend(reply.candidates);
        paths = reply.paths;
    }
    filtered(Reply { candidates, paths }, current)
}

/// Keep the candidates the typed prefix allows. Path directives survive the
/// filter — the shell applies the prefix to paths itself.
fn filtered(mut reply: Reply, prefix: &str) -> Reply {
    reply
        .candidates
        .retain(|candidate| candidate.value.starts_with(prefix));
    reply
}

/// The long options of one command level, `--help` included (it is a real
/// candidate for the user, unlike the hidden internals).
fn flags(cmd: &ClapCommand) -> Vec<Candidate> {
    cmd.get_arguments()
        .filter(|arg| !arg.is_hide_set() && !arg.is_positional())
        .filter_map(|arg| {
            arg.get_long().map(|long| {
                Candidate::new(
                    format!("--{long}"),
                    arg.get_help().map(|h| h.to_string()).as_deref(),
                )
            })
        })
        .collect()
}

/// What one argument accepts: its `ValueEnum` variants, a live list from the
/// system (apps, packages, sources, storages, credentials), or paths.
fn values_for(path: &[String], arg: &Arg, config: &Config) -> Reply {
    let possible = arg.get_possible_values();
    if !possible.is_empty() {
        return Reply::values(
            possible
                .iter()
                .filter(|value| !value.is_hide_set())
                .map(|value| {
                    Candidate::new(
                        value.get_name(),
                        value.get_help().map(|h| h.to_string()).as_deref(),
                    )
                })
                .collect(),
        );
    }
    if let Some(source) = dynamic_source(path, arg) {
        return Reply::values(source.candidates(config));
    }
    match arg.get_value_hint() {
        ValueHint::DirPath => Reply::paths(Paths::Dir),
        ValueHint::FilePath | ValueHint::AnyPath | ValueHint::ExecutablePath => {
            Reply::paths(Paths::File)
        }
        _ => Reply::empty(),
    }
}

/// A list of candidates that only exists at runtime.
enum Dynamic {
    /// Installed apps, by id.
    Apps,
    /// Packages in the registry index cache.
    Packages,
    /// Configured registry sources.
    Sources,
    /// Configured backup storages.
    Storages,
    /// Saved git/registry credentials, by pattern.
    Credentials,
}

/// Which live list, if any, fills an argument — keyed by where the argument
/// sits in the command tree and what it is called. Adding a command that takes
/// an app id as `id` or `app`, or a storage as `--storage`, needs no change
/// here; anything else does.
fn dynamic_source(path: &[String], arg: &Arg) -> Option<Dynamic> {
    let id = arg.get_id().as_str();
    let tail: Vec<&str> = path.iter().skip(1).map(String::as_str).collect();
    let dynamic = match (tail.as_slice(), id) {
        // `asc install <pkg>` / `asc search <query>` — registry names. The
        // spec may also be a git URL, which simply has no candidates.
        (["install"], "spec") | (["app", "install"], "spec") => Dynamic::Packages,
        (["search"], "query") => Dynamic::Packages,
        // `asc upgrade <app>` names an installed app, not a package.
        (["upgrade"], "spec") | (["app", "upgrade"], "spec") => Dynamic::Apps,
        (["source", "remove"], "name") => Dynamic::Sources,
        (["backup", "storage", "remove"], "name") => Dynamic::Storages,
        (["auth", "remove"], "target") => Dynamic::Credentials,
        (_, "source") => Dynamic::Sources,
        (_, "storage") | (_, "storages") => Dynamic::Storages,
        // Every command that addresses an app spells it `id` (`asc app stop
        // <id>`) or `app` (`asc backup create <app>`, `asc auth add --app`).
        (_, "id") | (_, "app") => Dynamic::Apps,
        _ => return None,
    };
    Some(dynamic)
}

impl Dynamic {
    fn candidates(&self, config: &Config) -> Vec<Candidate> {
        match self {
            Dynamic::Apps => apps(config),
            Dynamic::Packages => packages(config),
            Dynamic::Sources => sources(),
            Dynamic::Storages => storages(),
            Dynamic::Credentials => credentials(),
        }
    }
}

/// Installed apps: through the daemon when there is one (it is the only path
/// that sees another user's apps, and the only one a regular user can read at
/// all), else straight from the local app store.
fn apps(config: &Config) -> Vec<Candidate> {
    let config = config.clone();
    within_deadline(move || {
        if let Ok(Some(daemon)) = client::Daemon::connect(&config)
            && let Ok(apps) = daemon.list()
        {
            return apps
                .iter()
                .map(|app| Candidate::new(&app.id, Some(app.name.as_str())))
                .collect();
        }
        let manager = AppManager::new(&config);
        let ctx = UserContext::current();
        manager
            .list(&ctx)
            .map(|apps| {
                apps.iter()
                    .map(|app| Candidate::new(&app.meta.id, Some(app.meta.display_name())))
                    .collect()
            })
            .unwrap_or_default()
    })
    .unwrap_or_default()
}

/// Registry packages, cache only — a Tab press never fetches an index.
/// Stacks complete as `stack/app` too, since that is how a member installs.
fn packages(config: &Config) -> Vec<Candidate> {
    let config = config.clone();
    within_deadline(move || {
        let Ok(registry) = RegistryClient::new(&config) else {
            return Vec::new();
        };
        let mut candidates: Vec<Candidate> = registry
            .cached_packages()
            .into_iter()
            .map(|entry| {
                let help = entry.title.or(entry.description);
                Candidate::new(entry.name, help.as_deref())
            })
            .collect();
        candidates.sort_by(|a, b| a.value.cmp(&b.value));
        candidates.dedup_by(|a, b| a.value == b.value);
        candidates
    })
    .unwrap_or_default()
}

fn sources() -> Vec<Candidate> {
    within_deadline(|| {
        SourceList::load()
            .map(|list| {
                list.list()
                    .iter()
                    .map(|(source, scope)| Candidate::new(&source.name, Some(scope.label())))
                    .collect()
            })
            .unwrap_or_default()
    })
    .unwrap_or_default()
}

fn storages() -> Vec<Candidate> {
    within_deadline(|| {
        StorageList::load()
            .map(|list| {
                list.names()
                    .into_iter()
                    .map(|name| Candidate::new(name, None))
                    .collect()
            })
            .unwrap_or_default()
    })
    .unwrap_or_default()
}

fn credentials() -> Vec<Candidate> {
    within_deadline(|| {
        GitAuth::load()
            .map(|auth| {
                auth.list()
                    .iter()
                    .map(|(cred, _)| {
                        Candidate::new(&cred.pattern, Some(cred.method.label().as_str()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    })
    .unwrap_or_default()
}

/// Run a lookup with a hard time budget. The worker thread is left to finish
/// on its own if it overruns — the process is about to exit either way, and a
/// Tab press must not wait for a stuck daemon or a stalled filesystem.
fn within_deadline<T, F>(lookup: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // The receiver is gone once the deadline passed; the send simply fails.
        let _ = tx.send(lookup());
    });
    rx.recv_timeout(DEADLINE).ok()
}

/// The long option `name` at this command level, if it exists.
fn long_arg(cmd: &ClapCommand, name: &str) -> Option<Arg> {
    cmd.get_arguments()
        .find(|arg| arg.get_long() == Some(name))
        .cloned()
}

/// The short option `flag` at this command level, if it exists.
fn short_arg(cmd: &ClapCommand, flag: char) -> Option<Arg> {
    cmd.get_arguments()
        .find(|arg| arg.get_short() == Some(flag))
        .cloned()
}

/// Whether the option consumes the word after it. `SetTrue`/`Count`/`Help`
/// and friends do not; everything we complete values for is `Set`/`Append`.
fn takes_value(arg: &Arg) -> bool {
    matches!(arg.get_action(), ArgAction::Set | ArgAction::Append)
}

/// One line, no tabs, bounded width — clap help is prose and the wire format
/// is tab-separated.
fn trim_help(help: &str) -> String {
    let line = help.replace(['\n', '\t'], " ");
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(HELP_WIDTH) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        // A trailing space means "the cursor sits on a new, empty word".
        let mut words: Vec<String> = line.split_whitespace().map(str::to_string).collect();
        if line.ends_with(' ') {
            words.push(String::new());
        }
        words
    }

    fn values(line: &str) -> Vec<String> {
        let reply = complete(&words(line), &Config::default());
        reply.candidates.into_iter().map(|c| c.value).collect()
    }

    #[test]
    fn completes_top_level_commands() {
        let all = values("asc ");
        assert!(all.contains(&"install".to_string()));
        assert!(all.contains(&"backup".to_string()));
        // Hidden internals never surface.
        assert!(!all.contains(&"__complete".to_string()));
    }

    #[test]
    fn filters_by_the_typed_prefix() {
        assert_eq!(values("asc insta"), vec!["install".to_string()]);
    }

    #[test]
    fn completes_subcommands_of_a_group() {
        let actions = values("asc backup ");
        assert!(actions.contains(&"create".to_string()));
        assert!(actions.contains(&"storage".to_string()));
        // A deeper level resolves too.
        assert!(values("asc backup storage ").contains(&"add".to_string()));
    }

    #[test]
    fn completes_flags_of_the_current_level() {
        let flags = values("asc install foo --");
        assert!(flags.contains(&"--source".to_string()));
        assert!(flags.contains(&"--build".to_string()));
        assert!(flags.contains(&"--help".to_string()));
    }

    #[test]
    fn completes_value_enums() {
        assert_eq!(
            values("asc config debug "),
            vec!["on".to_string(), "off".to_string()]
        );
        let shells = values("asc completion ");
        assert!(shells.contains(&"bash".to_string()));
        assert!(shells.contains(&"fish".to_string()));
    }

    #[test]
    fn value_enums_complete_after_an_equals_sign() {
        assert_eq!(values("asc stats --sort=m"), vec!["--sort=mem".to_string()]);
    }

    #[test]
    fn a_flag_awaiting_a_value_wins_over_subcommands() {
        // `--sort <TAB>` completes the sort keys, not `asc`'s commands.
        assert_eq!(
            values("asc stats --sort "),
            vec!["cpu".to_string(), "mem".to_string()]
        );
    }

    #[test]
    fn path_arguments_defer_to_the_shell() {
        // `asc backup storage add --key <TAB>` is a private key path.
        let reply = complete(
            &words("asc backup storage add s3 --key "),
            &Config::default(),
        );
        assert!(reply.candidates.is_empty());
        assert_eq!(reply.paths, Paths::File);
    }

    #[test]
    fn positionals_are_counted_per_level() {
        // `asc backup restore <app> <backup>`: after the app, the next
        // positional is the backup name, which has no static candidates —
        // and, crucially, the app list is not offered again.
        let reply = complete(&words("asc backup restore myapp "), &Config::default());
        assert!(reply.candidates.is_empty());
        assert_eq!(reply.paths, Paths::None);
    }

    #[test]
    fn help_is_flattened_to_one_bounded_line() {
        let help = trim_help("first line\n\tsecond line");
        assert_eq!(help, "first line second line");
        assert!(trim_help(&"x".repeat(200)).ends_with('…'));
    }
}
