//! Terminal progress bars for long-running daemon operations (Docker image
//! pulls, git clones). Shown whenever stderr is a terminal — independent of
//! the tracing log level (`asc config debug`): the same events are always
//! logged through `tracing`, this module only adds a live visual on top for
//! interactive use, mirroring `docker pull`/`docker-compose pull` output.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::IsTerminal;
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

/// Whether progress bars should render. Log output goes to stderr
/// (`logging::init`), so bars share that stream and this check.
pub fn interactive() -> bool {
    std::io::stderr().is_terminal()
}

fn bytes_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{prefix:.bold.dim} [{bar:24.cyan/blue}] {bytes}/{total_bytes} {msg}",
    )
    .expect("static template")
    .progress_chars("=> ")
}

fn status_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:.bold.dim} {msg}").expect("static template")
}

/// A Docker status is terminal (the layer is done) once the Engine reports
/// one of these — the bar freezes there instead of resetting to a spinner.
fn layer_done(status: &str) -> bool {
    matches!(status, "Pull complete" | "Already exists")
}

/// One line per Docker image layer, `docker pull` / `docker-compose pull`
/// style: a status bar keyed by layer id, updated in place as the Engine
/// reports progress; frozen once the layer finishes.
pub struct LayerBars {
    multi: MultiProgress,
    bars: HashMap<String, ProgressBar>,
}

impl Default for LayerBars {
    fn default() -> Self {
        Self::new()
    }
}

impl LayerBars {
    pub fn new() -> Self {
        Self {
            multi: MultiProgress::new(),
            bars: HashMap::new(),
        }
    }

    /// A status line with no layer id (e.g. "Pulling from library/nginx",
    /// "Status: Downloaded newer image ...") — printed above the bars.
    pub fn header(&self, text: &str) {
        let _ = self.multi.println(text);
    }

    /// Update (creating on first sight) the line for one layer. `bytes` is
    /// `(current, total)` when the Engine reports byte progress (download or
    /// extract), `None` for status-only events (waiting, verifying, done).
    pub fn update(&mut self, layer: &str, status: &str, bytes: Option<(i64, i64)>) {
        if !self.bars.contains_key(layer) {
            // Style *before* prefix: each setter redraws immediately, and
            // with the order swapped the first redraw (from set_prefix)
            // still uses indicatif's default template — a full "0/0" bar
            // flashes for one frame before the real style ever applies.
            let pb = ProgressBar::new(0);
            pb.set_style(status_style());
            pb.set_prefix(layer.to_string());
            self.bars.insert(layer.to_string(), self.multi.add(pb));
        }
        let pb = self.bars.get(layer).expect("just inserted");
        match bytes {
            Some((current, total)) if total > 0 => {
                pb.set_style(bytes_style());
                pb.set_length(total as u64);
                pb.set_position(current.clamp(0, total) as u64);
                pb.set_message(String::new());
            }
            _ => {
                pb.set_style(status_style());
                pb.set_message(status.to_string());
            }
        }
        if layer_done(status) {
            pb.finish_with_message(status.to_string());
        }
    }

    /// Drop every bar once the pull is done — the terminal ones (`Pull
    /// complete`) stay on screen as the summary, matching `docker pull`.
    pub fn finish(self) {
        for pb in self.bars.values() {
            if !pb.is_finished() {
                pb.finish_and_clear();
            }
        }
    }
}

/// One spinner bar per BuildKit build step (`docker build`-style, DMN-050).
/// The Engine's BuildKit backend reports build progress as plain-text
/// trace lines of the form `#<step> [x/y] <description>` (the step's
/// label, first line seen for that step number) followed by `#<step>
/// <status>` sub-lines, terminated by `#<step> DONE <secs>s`, `#<step>
/// CACHED`, or `#<step> ERROR: <message>` — this mirrors what `docker
/// build` itself prints in non-interactive ("plain") progress mode. Lines
/// that don't match this shape (blank separators between step groups, or a
/// build that for some reason didn't go through BuildKit) are ignored.
pub struct BuildBars {
    multi: MultiProgress,
    bars: HashMap<u32, ProgressBar>,
}

impl Default for BuildBars {
    fn default() -> Self {
        Self::new()
    }
}

impl BuildBars {
    pub fn new() -> Self {
        Self {
            multi: MultiProgress::new(),
            bars: HashMap::new(),
        }
    }

    fn style() -> ProgressStyle {
        ProgressStyle::with_template("{spinner:.cyan} {prefix:.bold.dim} {msg}")
            .expect("static template")
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ")
    }

    /// Feed one line of the Engine's build output. The first line seen for a
    /// step number becomes that step's label (the bar's prefix); later lines
    /// update the transient status message until a terminal one freezes it.
    pub fn feed(&mut self, line: &str) {
        let Some(rest) = line.strip_prefix('#') else {
            return;
        };
        let Some((num, rest)) = rest.split_once(' ') else {
            return;
        };
        let Ok(step) = num.parse::<u32>() else {
            return;
        };
        let rest = rest.trim();
        if rest.is_empty() {
            return;
        }

        if let Some(pb) = self.bars.get(&step) {
            if let Some(secs) = rest.strip_prefix("DONE ") {
                pb.disable_steady_tick();
                pb.finish_with_message(format!("done {secs}"));
            } else if rest == "CACHED" {
                pb.disable_steady_tick();
                pb.finish_with_message("cached");
            } else if let Some(msg) = rest.strip_prefix("ERROR: ") {
                pb.disable_steady_tick();
                pb.finish_with_message(format!("error: {msg}"));
            } else {
                pb.set_message(rest.to_string());
            }
        } else {
            let pb = ProgressBar::new_spinner();
            pb.set_style(Self::style());
            pb.set_prefix(rest.to_string());
            pb.enable_steady_tick(Duration::from_millis(100));
            self.bars.insert(step, self.multi.add(pb));
        }
    }

    /// Drop every bar once the build is done — the terminal ones (`DONE`,
    /// `CACHED`) stay on screen as the summary, matching `docker build`.
    pub fn finish(self) {
        for pb in self.bars.values() {
            if !pb.is_finished() {
                pb.finish_and_clear();
            }
        }
    }
}

/// One line per registry index file (`asc update`): a spinner while the
/// fetch is in flight, frozen on its byte size or error once it lands. The
/// registry is a handful of small JSON files fetched one `curl` process at a
/// time (no connection reuse) — a bar stuck spinning on one file, rather
/// than a blank terminal, is what makes a stalled/throttled fetch visible
/// instead of indistinguishable from a frozen command.
/// `RefCell`-backed so it can be threaded through `&self` call chains
/// (`RegistryClient`'s methods don't take `&mut self`).
pub struct IndexBars {
    multi: MultiProgress,
    bars: RefCell<HashMap<String, ProgressBar>>,
}

impl Default for IndexBars {
    fn default() -> Self {
        Self::new()
    }
}

impl IndexBars {
    pub fn new() -> Self {
        Self {
            multi: MultiProgress::new(),
            bars: RefCell::new(HashMap::new()),
        }
    }

    fn spinner_style() -> ProgressStyle {
        ProgressStyle::with_template("{spinner:.cyan} {prefix:.bold.dim} {msg}")
            .expect("static template")
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ")
    }

    /// Start (and start spinning) the bar for one source/file pair.
    pub fn start(&self, source: &str, rel: &str) {
        let pb = ProgressBar::new_spinner();
        pb.set_style(Self::spinner_style());
        pb.set_prefix(format!("{source}/{rel}"));
        pb.set_message("fetching…");
        pb.enable_steady_tick(Duration::from_millis(100));
        self.bars
            .borrow_mut()
            .insert(rel.to_string(), self.multi.add(pb));
    }

    /// Freeze the bar on success — the byte count fetched.
    pub fn done(&self, rel: &str, bytes: usize) {
        if let Some(pb) = self.bars.borrow().get(rel) {
            pb.disable_steady_tick();
            pb.finish_with_message(format!("{bytes} B"));
        }
    }

    /// Freeze the bar on failure — the error stays on screen next to the
    /// file it belongs to instead of scrolling past in the error output.
    pub fn failed(&self, rel: &str, err: &str) {
        if let Some(pb) = self.bars.borrow().get(rel) {
            pb.disable_steady_tick();
            pb.finish_with_message(format!("failed: {err}"));
        }
    }
}

/// A plain spinner with a static label for operations that report no
/// incremental progress (`asc app start/stop/restart` waiting on the
/// runtime or the daemon): the user sees the command is alive instead of a
/// silent terminal. Cleared without a trace once the operation finishes —
/// the result line is the summary.
pub struct Spinner(ProgressBar);

impl Spinner {
    pub fn new(label: &str) -> Self {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg}")
                .expect("static template")
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "),
        );
        pb.set_message(label.to_string());
        pb.enable_steady_tick(Duration::from_millis(100));
        Self(pb)
    }

    pub fn finish(self) {
        self.0.finish_and_clear();
    }
}

/// A single phase/percent bar for `git clone --progress` (Enumerating,
/// Counting, Compressing, Receiving, Resolving deltas).
pub struct PhaseBar(ProgressBar);

impl Default for PhaseBar {
    fn default() -> Self {
        Self::new()
    }
}

impl PhaseBar {
    pub fn new() -> Self {
        let pb = ProgressBar::new(100);
        pb.set_style(
            ProgressStyle::with_template("{msg} [{bar:24.cyan/blue}] {percent}%")
                .expect("static template")
                .progress_chars("=> "),
        );
        Self(pb)
    }

    pub fn update(&self, phase: &str, pct: u8) {
        self.0.set_message(phase.to_string());
        self.0.set_position(u64::from(pct));
    }

    pub fn finish(&self) {
        self.0.finish_and_clear();
    }
}

/// A single byte-progress bar for directory copies (`asc app clone`).
pub struct CopyBar(ProgressBar);

impl CopyBar {
    pub fn new(total_bytes: u64) -> Self {
        let pb = ProgressBar::new(total_bytes);
        pb.set_style(bytes_style());
        pb.set_prefix("Copying");
        Self(pb)
    }

    pub fn set_length(&self, total_bytes: u64) {
        self.0.set_length(total_bytes);
    }

    pub fn set_position(&self, bytes: u64) {
        self.0.set_position(bytes);
    }

    pub fn finish(&self) {
        self.0.finish_and_clear();
    }
}

/// Parse a `git clone --progress` line, e.g. `Receiving objects:  45%
/// (450/1000), 12.34 MiB | 1.02 MiB/s` into `("Receiving objects", 45)`.
/// The server-side phases (Enumerating/Counting/Compressing objects) arrive
/// prefixed with `remote: `, which is stripped before parsing. `None` for
/// lines without a percentage (e.g. `Enumerating objects: 758, done.`, or
/// plain lines like `Cloning into 'x'...`).
pub fn parse_git_progress(line: &str) -> Option<(&str, u8)> {
    let line = line.strip_prefix("remote: ").unwrap_or(line);
    let (phase, rest) = line.split_once(':')?;
    let token = rest.split_whitespace().next()?;
    let pct = token.strip_suffix('%')?.parse::<u8>().ok()?;
    Some((phase.trim(), pct))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_percent_lines() {
        assert_eq!(
            parse_git_progress("Receiving objects:  45% (450/1000), 12.34 MiB | 1.02 MiB/s"),
            Some(("Receiving objects", 45))
        );
        assert_eq!(
            parse_git_progress("Counting objects: 100% (100/100), done."),
            Some(("Counting objects", 100))
        );
        assert_eq!(
            parse_git_progress("Resolving deltas:   0% (0/40)"),
            Some(("Resolving deltas", 0))
        );
        // Server-side phases arrive "remote: "-prefixed (verified against a
        // real `git clone --progress` capture).
        assert_eq!(
            parse_git_progress("remote: Counting objects:  33% (1/3)        "),
            Some(("Counting objects", 33))
        );
    }

    #[test]
    fn ignores_lines_without_a_percentage() {
        assert_eq!(parse_git_progress("Enumerating objects: 758, done."), None);
        assert_eq!(parse_git_progress("Cloning into 'cs2-server'..."), None);
        assert_eq!(parse_git_progress("remote: Total 758 (delta 0)"), None);
    }
}
