//! A small glob matcher for `asc.backup.yaml` exclude patterns — no crate
//! pulled in for this alone. Supports `*` (any run of characters, including
//! none, but never crossing a `/`), `**` (any run of characters, `/`
//! included) and `?` (exactly one character, never `/`); everything else
//! matches literally. Patterns are matched against the backup-relative path
//! (forward slashes, no leading `/`).

/// Whether `path` matches `pattern`.
pub fn matches(pattern: &str, path: &str) -> bool {
    match_bytes(pattern.as_bytes(), path.as_bytes())
}

fn match_bytes(pattern: &[u8], path: &[u8]) -> bool {
    match pattern.first() {
        None => path.is_empty(),
        Some(b'*') => {
            if pattern.get(1) == Some(&b'*') {
                let rest = &pattern[2..];
                // `**` also eats a following '/' so `a/**/b` matches `a/b`.
                let rest = rest.strip_prefix(b"/").unwrap_or(rest);
                (0..=path.len()).any(|i| match_bytes(rest, &path[i..]))
            } else {
                let rest = &pattern[1..];
                (0..=path.len())
                    .take_while(|&i| i == 0 || path[i - 1] != b'/')
                    .any(|i| match_bytes(rest, &path[i..]))
            }
        }
        Some(b'?') => !path.is_empty() && path[0] != b'/' && match_bytes(&pattern[1..], &path[1..]),
        Some(&c) => !path.is_empty() && path[0] == c && match_bytes(&pattern[1..], &path[1..]),
    }
}

/// Whether `path` matches any of `patterns`, or any of their ancestor
/// directories does (excluding a directory excludes everything under it,
/// same as `.gitignore`).
pub fn matches_any(patterns: &[String], path: &str) -> bool {
    patterns.iter().any(|p| {
        matches(p, path)
            || path
                .rmatch_indices('/')
                .any(|(i, _)| matches(p, &path[..i]))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_and_question_mark() {
        assert!(matches("cache.log", "cache.log"));
        assert!(!matches("cache.log", "cache.log.bak"));
        assert!(matches("cache.???", "cache.log"));
        assert!(!matches("cache.???", "cache.logs"));
    }

    #[test]
    fn single_star_stops_at_slash() {
        assert!(matches("*.log", "server.log"));
        assert!(!matches("*.log", "logs/server.log"));
        assert!(matches("logs/*.log", "logs/server.log"));
        assert!(!matches("logs/*.log", "logs/old/server.log"));
    }

    #[test]
    fn double_star_crosses_slashes() {
        assert!(matches("cache/**", "cache/a/b/c.tmp"));
        assert!(matches("**/*.tmp", "a/b/c.tmp"));
        assert!(matches("**/*.tmp", "c.tmp"));
        assert!(!matches("cache/**", "other/a.tmp"));
    }

    #[test]
    fn excluding_a_directory_excludes_its_contents() {
        let patterns = ["cache".to_string()];
        assert!(matches_any(&patterns, "cache"));
        assert!(matches_any(&patterns, "cache/a/b.txt"));
        assert!(!matches_any(&patterns, "other/cache/b.txt"));
    }
}
