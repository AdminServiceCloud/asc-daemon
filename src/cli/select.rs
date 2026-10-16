//! Arrow-key picker for interactive CLI choices: `↑`/`↓` (or `k`/`j`) move,
//! `Enter` picks, `Esc`/`q`/`Ctrl+C` cancel. A small termios raw-mode reader
//! on top of `libc` — no terminal UI dependency — that redraws the list in
//! place with ANSI cursor movement and restores the terminal on every exit
//! path (the guard's `Drop` runs on an early return and on a panic unwind).
//!
//! Key decoding and the cursor are pure functions so they are unit-tested
//! without a terminal. Callers check that stdin is a terminal first; a
//! stdout that is not one, or `TERM=dumb`, gets a numbered list instead.

use std::io::{self, IsTerminal, Write};

use asc_daemon::daemon::i18n::{Msg, t};

/// One decoded keypress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    Up,
    Down,
    Enter,
    Cancel,
    Other,
}

/// Decode the key at the start of `bytes`, returning it and how many bytes
/// it took. Arrows come as CSI (`ESC [ A`) or SS3 (`ESC O A`) depending on
/// the terminal's cursor-key mode; any other escape sequence is consumed
/// whole and ignored, so e.g. `←` never leaks its `[D` as two more keys. A
/// lone `ESC` is the Esc key itself. Non-ASCII bytes are skipped one at a
/// time, which is harmless: none of them is a key this picker reacts to.
pub(crate) fn decode(bytes: &[u8]) -> (Key, usize) {
    match bytes {
        [] => (Key::Other, 0),
        [0x1b, b'[' | b'O', b'A', ..] => (Key::Up, 3),
        [0x1b, b'[' | b'O', b'B', ..] => (Key::Down, 3),
        [0x1b, b'O', _, ..] => (Key::Other, 3),
        [0x1b, b'[', rest @ ..] => {
            // CSI: parameter/intermediate bytes, then one final byte.
            let len = rest
                .iter()
                .position(|b| (0x40..=0x7e).contains(b))
                .map_or(bytes.len(), |end| end + 3);
            (Key::Other, len)
        }
        [0x1b, ..] => (Key::Cancel, 1),
        [b'\r' | b'\n', ..] => (Key::Enter, 1),
        [0x03 | 0x04 | b'q' | b'Q', ..] => (Key::Cancel, 1),
        [b'k', ..] => (Key::Up, 1),
        [b'j', ..] => (Key::Down, 1),
        [_, ..] => (Key::Other, 1),
    }
}

/// What a key did to the picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    Moved,
    Picked(usize),
    Cancelled,
    Ignored,
}

/// The highlighted row of a list of `len` items, wrapping at both ends.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Cursor {
    pub index: usize,
    len: usize,
}

impl Cursor {
    pub fn new(len: usize, index: usize) -> Self {
        Self {
            index: index.min(len.saturating_sub(1)),
            len,
        }
    }

    pub fn apply(&mut self, key: Key) -> Step {
        match key {
            Key::Up if self.len > 0 => {
                self.index = (self.index + self.len - 1) % self.len;
                Step::Moved
            }
            Key::Down if self.len > 0 => {
                self.index = (self.index + 1) % self.len;
                Step::Moved
            }
            Key::Enter if self.len > 0 => Step::Picked(self.index),
            Key::Cancel => Step::Cancelled,
            _ => Step::Ignored,
        }
    }
}

/// Let the user pick one of `items` under `prompt`, `default` preselected.
/// `Ok(None)` = cancelled. Stdin must be a terminal (the caller's check).
pub fn select(prompt: &str, items: &[String], default: usize) -> anyhow::Result<Option<usize>> {
    if items.is_empty() {
        return Ok(None);
    }
    let dumb = std::env::var("TERM").is_ok_and(|term| term == "dumb");
    if dumb || !io::stdout().is_terminal() {
        return select_numbered(prompt, items, default);
    }
    let color = std::env::var_os("NO_COLOR").is_none();
    let mut out = io::stdout().lock();
    writeln!(out, "{prompt}")?;
    let width = terminal_width();
    let raw = RawMode::enable()?;
    let mut cursor = Cursor::new(items.len(), default);
    render(&mut out, items, cursor.index, width, color, false)?;
    let mut buf = [0u8; 32];
    let outcome = 'read: loop {
        let n = read_keys(&mut buf)?;
        if n == 0 {
            // EOF on the terminal: nothing more will ever be typed.
            break None;
        }
        let mut at = 0;
        while at < n {
            let (key, used) = decode(&buf[at..n]);
            at += used.max(1);
            match cursor.apply(key) {
                Step::Moved => render(&mut out, items, cursor.index, width, color, true)?,
                Step::Picked(index) => break 'read Some(index),
                Step::Cancelled => break 'read None,
                Step::Ignored => {}
            }
        }
    };
    // Collapse the list into the one line that was picked.
    clear(&mut out, items.len() + 1)?;
    if let Some(index) = outcome {
        let line = truncate(&items[index], width);
        if color {
            writeln!(out, "  \x1b[36m✔ {line}\x1b[0m")?;
        } else {
            writeln!(out, "  ✔ {line}")?;
        }
    }
    out.flush()?;
    drop(raw);
    Ok(outcome)
}

/// Draw the list plus the key hint; `redraw` first moves back over the
/// previous drawing. Every line is cut to the terminal width, or a wrapped
/// line would throw the cursor-up count off.
fn render(
    out: &mut impl Write,
    items: &[String],
    selected: usize,
    width: usize,
    color: bool,
    redraw: bool,
) -> io::Result<()> {
    if redraw {
        clear(out, items.len() + 1)?;
    }
    for (i, item) in items.iter().enumerate() {
        let line = truncate(item, width);
        match (i == selected, color) {
            (true, true) => write!(out, "\x1b[36m❯ {line}\x1b[0m\r\n")?,
            (true, false) => write!(out, "❯ {line}\r\n")?,
            (false, _) => write!(out, "  {line}\r\n")?,
        }
    }
    let hint = truncate(t(Msg::SelectHint), width);
    if color {
        write!(out, "\x1b[2m  {hint}\x1b[0m\r\n")?;
    } else {
        write!(out, "  {hint}\r\n")?;
    }
    out.flush()
}

/// Move up over `lines` drawn lines and erase from there to the end of the
/// screen.
fn clear(out: &mut impl Write, lines: usize) -> io::Result<()> {
    write!(out, "\x1b[{lines}A\r\x1b[J")
}

/// `text` cut to what fits after the two-column marker on a `width`-wide
/// terminal (characters, not display cells — close enough for the labels
/// this picker shows).
fn truncate(text: &str, width: usize) -> String {
    let room = width.saturating_sub(3).max(10);
    if text.chars().count() <= room {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(room - 1).collect();
    cut.push('…');
    cut
}

fn terminal_width() -> usize {
    // SAFETY: TIOCGWINSZ fills a winsize on success; it is zeroed otherwise.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    if ok && size.ws_col > 0 {
        usize::from(size.ws_col)
    } else {
        80
    }
}

/// Read whatever keys are available into `buf`, blocking for the first byte.
/// An escape sequence split across reads (a slow link, a multiplexer) is
/// completed by waiting briefly for its tail, so a lone `ESC` is told apart
/// from the start of an arrow.
fn read_keys(buf: &mut [u8]) -> io::Result<usize> {
    let mut n = read_stdin(buf)?;
    while n > 0 && n < buf.len() && incomplete_escape(&buf[..n]) {
        if !stdin_ready(30)? {
            break;
        }
        let more = read_stdin(&mut buf[n..])?;
        if more == 0 {
            break;
        }
        n += more;
    }
    Ok(n)
}

/// Whether `bytes` ends in an escape sequence that has not been finished.
fn incomplete_escape(bytes: &[u8]) -> bool {
    let Some(esc) = bytes.iter().rposition(|&b| b == 0x1b) else {
        return false;
    };
    match &bytes[esc..] {
        [0x1b] | [0x1b, b'[' | b'O'] => true,
        [0x1b, b'[', rest @ ..] => !rest.iter().any(|b| (0x40..=0x7e).contains(b)),
        _ => false,
    }
}

fn read_stdin(buf: &mut [u8]) -> io::Result<usize> {
    loop {
        // SAFETY: `buf` is valid for `buf.len()` writable bytes.
        let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

fn stdin_ready(timeout_ms: i32) -> io::Result<bool> {
    let mut fd = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    let n = unsafe { libc::poll(&mut fd, 1, timeout_ms) };
    if n < 0 {
        let err = io::Error::last_os_error();
        return if err.kind() == io::ErrorKind::Interrupted {
            Ok(false)
        } else {
            Err(err)
        };
    }
    Ok(n > 0)
}

/// Raw input on stdin for as long as the guard lives: no line buffering, no
/// echo, and no signal keys — `Ctrl+C` arrives as a byte the picker treats
/// as cancel, so the terminal is restored by `Drop` instead of being left
/// raw by a SIGINT that kills the process mid-pick. The cursor is hidden
/// while the list is shown.
struct RawMode {
    original: libc::termios,
}

impl RawMode {
    fn enable() -> io::Result<Self> {
        // SAFETY: tcgetattr fills the termios on success, checked below.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = original;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        // SAFETY: `raw` is a valid termios derived from the current one.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut out = io::stdout();
        let _ = write!(out, "\x1b[?25l");
        let _ = out.flush();
        Ok(Self { original })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restores the termios read in `enable`.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.original) };
        let mut out = io::stdout();
        let _ = write!(out, "\x1b[?25h");
        let _ = out.flush();
    }
}

/// The non-ANSI fallback: a numbered list and a typed answer; an empty
/// answer takes the preselected item, `q` cancels.
fn select_numbered(
    prompt: &str,
    items: &[String],
    default: usize,
) -> anyhow::Result<Option<usize>> {
    println!("{prompt}");
    for (i, item) in items.iter().enumerate() {
        let marker = if i == default { '*' } else { ' ' };
        println!(" {marker}{}) {item}", i + 1);
    }
    print!("> ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let answer = line.trim();
    if answer.is_empty() {
        return Ok(Some(default.min(items.len() - 1)));
    }
    if answer.eq_ignore_ascii_case("q") {
        return Ok(None);
    }
    answer
        .parse::<usize>()
        .ok()
        .filter(|n| (1..=items.len()).contains(n))
        .map(|n| Some(n - 1))
        .ok_or_else(|| anyhow::anyhow!(t(Msg::AuthInvalidChoice)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_arrows_in_both_cursor_key_modes() {
        assert_eq!(decode(b"\x1b[A"), (Key::Up, 3));
        assert_eq!(decode(b"\x1b[B"), (Key::Down, 3));
        assert_eq!(decode(b"\x1bOA"), (Key::Up, 3));
        assert_eq!(decode(b"\x1bOB"), (Key::Down, 3));
        assert_eq!(decode(b"k"), (Key::Up, 1));
        assert_eq!(decode(b"j"), (Key::Down, 1));
    }

    #[test]
    fn decodes_enter_and_cancel_keys() {
        assert_eq!(decode(b"\r"), (Key::Enter, 1));
        assert_eq!(decode(b"\n"), (Key::Enter, 1));
        assert_eq!(decode(b"\x1b"), (Key::Cancel, 1));
        assert_eq!(decode(b"\x03"), (Key::Cancel, 1));
        assert_eq!(decode(b"q"), (Key::Cancel, 1));
    }

    #[test]
    fn other_escape_sequences_are_consumed_whole() {
        // Left arrow, Delete (`ESC [ 3 ~`), F1 in SS3 form.
        assert_eq!(decode(b"\x1b[D"), (Key::Other, 3));
        assert_eq!(decode(b"\x1b[3~j"), (Key::Other, 4));
        assert_eq!(decode(b"\x1bOP"), (Key::Other, 3));
        // Unfinished CSI: everything that is there is swallowed.
        assert_eq!(decode(b"\x1b[12"), (Key::Other, 4));
        assert_eq!(decode("ж".as_bytes()), (Key::Other, 1));
    }

    #[test]
    fn several_keys_in_one_read_decode_in_order() {
        let bytes = b"\x1b[B\x1b[Bj\r";
        let mut at = 0;
        let mut keys = Vec::new();
        while at < bytes.len() {
            let (key, used) = decode(&bytes[at..]);
            keys.push(key);
            at += used;
        }
        assert_eq!(keys, [Key::Down, Key::Down, Key::Down, Key::Enter]);
    }

    #[test]
    fn incomplete_escapes_are_recognized() {
        assert!(incomplete_escape(b"\x1b"));
        assert!(incomplete_escape(b"\x1b["));
        assert!(incomplete_escape(b"\x1bO"));
        assert!(incomplete_escape(b"j\x1b[1;5"));
        assert!(!incomplete_escape(b"\x1b[A"));
        assert!(!incomplete_escape(b"jk"));
    }

    #[test]
    fn cursor_wraps_and_picks() {
        let mut cursor = Cursor::new(3, 0);
        assert_eq!(cursor.apply(Key::Up), Step::Moved);
        assert_eq!(cursor.index, 2);
        assert_eq!(cursor.apply(Key::Down), Step::Moved);
        assert_eq!(cursor.index, 0);
        cursor.apply(Key::Down);
        assert_eq!(cursor.apply(Key::Enter), Step::Picked(1));
        assert_eq!(cursor.apply(Key::Other), Step::Ignored);
        assert_eq!(cursor.apply(Key::Cancel), Step::Cancelled);
    }

    #[test]
    fn cursor_clamps_an_out_of_range_default() {
        assert_eq!(Cursor::new(2, 9).index, 1);
        let mut empty = Cursor::new(0, 0);
        assert_eq!(empty.apply(Key::Enter), Step::Ignored);
        assert_eq!(empty.apply(Key::Down), Step::Ignored);
    }

    #[test]
    fn long_labels_are_cut_to_the_terminal() {
        assert_eq!(truncate("short", 80), "short");
        let cut = truncate(&"x".repeat(100), 20);
        assert_eq!(cut.chars().count(), 17);
        assert!(cut.ends_with('…'));
    }
}
