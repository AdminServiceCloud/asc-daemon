//! Talking to the kernel through the `nft` binary. Behind a trait
//! so the apply/rollback logic is testable without a kernel, and so a future
//! libnftables binding can replace the process calls.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::daemon::exec::{run_captured, run_with_input};

/// Longest table text `ListTables` returns per table.
pub const MAX_TABLE_TEXT: usize = 64 * 1024;

pub trait Nft: Send + Sync {
    fn installed(&self) -> bool;
    /// The absolute path of the binary, for the boot unit.
    fn path(&self) -> String;
    fn version(&self) -> String;
    /// `nft -c -f -`: parse and check without touching the kernel.
    fn check(&self, script: &str) -> Result<()>;
    /// `nft -f -`: apply the script as one transaction.
    fn apply(&self, script: &str) -> Result<()>;
    /// `nft list ruleset`.
    fn list_ruleset(&self) -> Result<String>;
    /// `nft list table <family> <name>`; `None` when the table does not exist.
    fn list_table(&self, family: &str, name: &str) -> Result<Option<String>>;
    /// `nft -j list table <family> <name>` (counters); `None` when absent.
    fn table_json(&self, family: &str, name: &str) -> Result<Option<String>>;
    /// `nft -j list tables`.
    fn tables_json(&self) -> Result<String>;
}

/// The real thing.
pub struct SystemNft;

impl SystemNft {
    /// The absolute path of `nft` (systemd needs one for `ExecStart`).
    fn binary() -> Option<PathBuf> {
        let from_path = std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("nft"))
                .find(|candidate| candidate.is_file())
        });
        from_path.or_else(|| {
            [
                "/usr/sbin/nft",
                "/sbin/nft",
                "/usr/bin/nft",
                "/usr/local/sbin/nft",
            ]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
        })
    }

    fn run(&self, args: &[&str]) -> Result<(bool, String)> {
        let Some(bin) = Self::binary() else {
            bail!("nftables is not installed (no `nft` binary)");
        };
        run_captured(&bin.to_string_lossy(), args)
    }

    fn run_input(&self, args: &[&str], input: &str) -> Result<(bool, String)> {
        let Some(bin) = Self::binary() else {
            bail!("nftables is not installed (no `nft` binary)");
        };
        run_with_input(&bin.to_string_lossy(), args, input)
    }
}

impl Nft for SystemNft {
    fn installed(&self) -> bool {
        Self::binary().is_some()
    }

    fn path(&self) -> String {
        Self::binary()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/usr/sbin/nft".into())
    }

    fn version(&self) -> String {
        self.run(&["--version"])
            .ok()
            .filter(|(ok, _)| *ok)
            .map(|(_, out)| parse_version(&out))
            .unwrap_or_default()
    }

    fn check(&self, script: &str) -> Result<()> {
        let (ok, out) = self.run_input(&["-c", "-f", "-"], script)?;
        if !ok {
            bail!("nft rejected the ruleset: {}", out.trim());
        }
        Ok(())
    }

    fn apply(&self, script: &str) -> Result<()> {
        let (ok, out) = self.run_input(&["-f", "-"], script)?;
        if !ok {
            bail!("nft could not apply the ruleset: {}", out.trim());
        }
        Ok(())
    }

    fn list_ruleset(&self) -> Result<String> {
        let (ok, out) = self.run(&["list", "ruleset"])?;
        if !ok {
            bail!("nft list ruleset failed: {}", out.trim());
        }
        Ok(out)
    }

    fn list_table(&self, family: &str, name: &str) -> Result<Option<String>> {
        let (ok, out) = self.run(&["list", "table", family, name])?;
        table_result(ok, out)
    }

    fn table_json(&self, family: &str, name: &str) -> Result<Option<String>> {
        let (ok, out) = self.run(&["-j", "list", "table", family, name])?;
        table_result(ok, out)
    }

    fn tables_json(&self) -> Result<String> {
        let (ok, out) = self.run(&["-j", "list", "tables"])?;
        if !ok {
            bail!("nft list tables failed: {}", out.trim());
        }
        Ok(out)
    }
}

fn table_result(ok: bool, out: String) -> Result<Option<String>> {
    if ok {
        return Ok(Some(out));
    }
    // A missing table is the normal "not applied yet" state, not an error.
    if out.contains("No such file or directory") || out.contains("does not exist") {
        return Ok(None);
    }
    bail!("nft failed: {}", out.trim())
}

/// `nftables v1.0.9 (Old Doc Yammer)` → `1.0.9`.
pub fn parse_version(output: &str) -> String {
    output
        .split_whitespace()
        .find_map(|word| word.strip_prefix('v'))
        .map(|v| {
            v.chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect::<String>()
        })
        .unwrap_or_default()
}

/// Packets and bytes a rule has matched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct Counter {
    pub packets: u64,
    pub bytes: u64,
}

/// Sums the counters of `nft -j list table` by the `asc:<id>` comment of
/// the rule. One model rule can be several nft rules (one per source family),
/// so the lines with the same id add up.
pub fn parse_counters(json: &str) -> BTreeMap<String, Counter> {
    let mut out: BTreeMap<String, Counter> = BTreeMap::new();
    let Ok(doc) = serde_json::from_str::<Value>(json) else {
        return out;
    };
    let Some(items) = doc.get("nftables").and_then(Value::as_array) else {
        return out;
    };
    for item in items {
        let Some(rule) = item.get("rule") else {
            continue;
        };
        let Some(id) = rule
            .get("comment")
            .and_then(Value::as_str)
            .and_then(|c| c.strip_prefix("asc:"))
        else {
            continue;
        };
        let counter = rule
            .get("expr")
            .and_then(Value::as_array)
            .and_then(|exprs| exprs.iter().find_map(|e| e.get("counter")));
        let Some(counter) = counter else { continue };
        let entry = out.entry(id.to_string()).or_default();
        entry.packets += counter.get("packets").and_then(Value::as_u64).unwrap_or(0);
        entry.bytes += counter.get("bytes").and_then(Value::as_u64).unwrap_or(0);
    }
    out
}

/// `(family, name)` of every table in `nft -j list tables`.
pub fn parse_tables(json: &str) -> Vec<(String, String)> {
    let Ok(doc) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    doc.get("nftables")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let table = item.get("table")?;
                    Some((
                        table.get("family")?.as_str()?.to_string(),
                        table.get("name")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A scripted [`Nft`] for tests: records what was applied and can be told to
/// fail.
#[cfg(test)]
pub mod fake {
    use std::sync::Mutex;

    use anyhow::{Result, bail};

    use super::Nft;

    #[derive(Default)]
    pub struct FakeNft {
        pub applied: Mutex<Vec<String>>,
        pub checked: Mutex<Vec<String>>,
        /// What `list_table` answers; `None` for "no such table".
        pub table: Mutex<Option<String>>,
        pub ruleset: Mutex<String>,
        /// A script containing this text is refused by `check` and `apply`.
        pub reject: Mutex<Option<String>>,
        pub missing: bool,
    }

    impl FakeNft {
        pub fn applied(&self) -> Vec<String> {
            self.applied.lock().unwrap().clone()
        }
    }

    impl Nft for FakeNft {
        fn installed(&self) -> bool {
            !self.missing
        }

        fn path(&self) -> String {
            "/usr/sbin/nft".into()
        }

        fn version(&self) -> String {
            "1.0.9".into()
        }

        fn check(&self, script: &str) -> Result<()> {
            self.checked.lock().unwrap().push(script.to_string());
            if let Some(bad) = self.reject.lock().unwrap().as_deref()
                && script.contains(bad)
            {
                bail!("nft rejected the ruleset: syntax error");
            }
            Ok(())
        }

        fn apply(&self, script: &str) -> Result<()> {
            self.check(script)?;
            self.applied.lock().unwrap().push(script.to_string());
            Ok(())
        }

        fn list_ruleset(&self) -> Result<String> {
            Ok(self.ruleset.lock().unwrap().clone())
        }

        fn list_table(&self, _: &str, _: &str) -> Result<Option<String>> {
            Ok(self.table.lock().unwrap().clone())
        }

        fn table_json(&self, _: &str, _: &str) -> Result<Option<String>> {
            Ok(None)
        }

        fn tables_json(&self) -> Result<String> {
            Ok("{\"nftables\":[]}".into())
        }
    }
    /// Tests keep a handle on the fake while the firewall owns the box.
    impl Nft for std::sync::Arc<FakeNft> {
        fn installed(&self) -> bool {
            (**self).installed()
        }

        fn path(&self) -> String {
            (**self).path()
        }

        fn version(&self) -> String {
            (**self).version()
        }

        fn check(&self, script: &str) -> Result<()> {
            (**self).check(script)
        }

        fn apply(&self, script: &str) -> Result<()> {
            (**self).apply(script)
        }

        fn list_ruleset(&self) -> Result<String> {
            (**self).list_ruleset()
        }

        fn list_table(&self, family: &str, name: &str) -> Result<Option<String>> {
            (**self).list_table(family, name)
        }

        fn table_json(&self, family: &str, name: &str) -> Result<Option<String>> {
            (**self).table_json(family, name)
        }

        fn tables_json(&self) -> Result<String> {
            (**self).tables_json()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_parsed_from_the_banner() {
        assert_eq!(parse_version("nftables v1.0.9 (Old Doc Yammer)\n"), "1.0.9");
        assert_eq!(parse_version("garbage"), "");
    }

    #[test]
    fn counters_are_summed_per_rule_id() {
        let json = r#"{"nftables":[
            {"metainfo":{"version":"1.0.9"}},
            {"table":{"family":"inet","name":"asc","handle":1}},
            {"rule":{"family":"inet","table":"asc","chain":"input","handle":5,"comment":"asc:web",
                     "expr":[{"match":{}},{"counter":{"packets":3,"bytes":180}},{"accept":null}]}},
            {"rule":{"family":"inet","table":"asc","chain":"input","handle":6,"comment":"asc:web",
                     "expr":[{"counter":{"packets":2,"bytes":20}},{"accept":null}]}},
            {"rule":{"family":"inet","table":"asc","chain":"input","handle":7,"comment":"other",
                     "expr":[{"counter":{"packets":9,"bytes":9}}]}},
            {"rule":{"family":"inet","table":"asc","chain":"input","handle":8,
                     "expr":[{"counter":{"packets":1,"bytes":1}}]}}
        ]}"#;
        let counters = parse_counters(json);
        assert_eq!(counters.len(), 1);
        assert_eq!(
            counters["web"],
            Counter {
                packets: 5,
                bytes: 200
            }
        );
    }

    #[test]
    fn broken_json_gives_no_counters() {
        assert!(parse_counters("not json").is_empty());
        assert!(parse_counters("{}").is_empty());
    }

    #[test]
    fn tables_are_listed_with_their_family() {
        let json = r#"{"nftables":[{"metainfo":{}},
            {"table":{"family":"inet","name":"asc","handle":1}},
            {"table":{"family":"ip","name":"nat","handle":2}}]}"#;
        assert_eq!(
            parse_tables(json),
            [
                ("inet".to_string(), "asc".to_string()),
                ("ip".to_string(), "nat".to_string())
            ]
        );
    }

    #[test]
    fn a_missing_table_is_not_an_error() {
        assert_eq!(
            table_result(false, "Error: No such file or directory".into()).unwrap(),
            None
        );
        assert!(table_result(false, "Error: Operation not permitted".into()).is_err());
        assert_eq!(
            table_result(true, "table".into()).unwrap(),
            Some("table".into())
        );
    }
}
