//! Incremental change tracking through git.
//!
//! Every tick the tracker asks git which files are dirty — in the root
//! repository and in every initialized submodule — fingerprints them and
//! compares the result with the previous tick. Only the difference is
//! published, so an idle workspace costs a couple of `git` invocations.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, bail};
use brok_sdk::payload::FileChanged;
use tokio::process::Command;

/// Cheap identity of a dirty file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fingerprint {
    /// The file exists with this size and modification time.
    Present {
        /// Size in bytes.
        len: u64,
        /// Modification time, nanoseconds since the epoch.
        modified_ns: u128,
    },
    /// The file is tracked but missing from disk.
    Deleted,
}

/// Dirty files by workspace relative path (`/` separators).
pub type Snapshot = BTreeMap<String, Fingerprint>;

/// Computes what changed between two snapshots.
///
/// - new or re-fingerprinted dirty paths are reported as changed (or deleted);
/// - paths that were dirty and are now clean were reverted or committed: they
///   are reported as changed when they still exist (`exists`), deleted otherwise.
pub fn diff(previous: &Snapshot, current: &Snapshot, exists: impl Fn(&str) -> bool) -> FileChanged {
    let mut change = FileChanged::default();
    for (path, fingerprint) in current {
        if previous.get(path) == Some(fingerprint) {
            continue;
        }
        match fingerprint {
            Fingerprint::Deleted => change.deleted.push(path.clone()),
            Fingerprint::Present { .. } => change.changed.push(path.clone()),
        }
    }
    for path in previous.keys().filter(|p| !current.contains_key(*p)) {
        if exists(path) { change.changed.push(path.clone()) } else { change.deleted.push(path.clone()) }
    }
    change.changed.sort();
    change.deleted.sort();
    change
}

/// Polls git and reports incremental changes.
#[derive(Debug)]
pub struct Tracker {
    root: PathBuf,
    previous: Snapshot,
}

impl Tracker {
    /// Tracker for the workspace at `root`. The first poll reports every dirty file.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), previous: Snapshot::new() }
    }

    /// Scans the workspace and returns what changed since the last poll.
    pub async fn poll(&mut self) -> anyhow::Result<FileChanged> {
        let mut current = Snapshot::new();
        for path in dirty_paths(&self.root).await? {
            current.insert(path.clone(), fingerprint(&self.root.join(&path)));
        }
        let root = self.root.clone();
        let change = diff(&self.previous, &current, |p| root.join(p).is_file());
        self.previous = current;
        Ok(change)
    }
}

fn fingerprint(path: &Path) -> Fingerprint {
    match std::fs::metadata(path) {
        Ok(meta) => Fingerprint::Present {
            len: meta.len(),
            modified_ns: meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos()),
        },
        Err(_) => Fingerprint::Deleted,
    }
}

/// Dirty files of the root repository and every initialized submodule.
async fn dirty_paths(root: &Path) -> anyhow::Result<BTreeSet<String>> {
    let mut repos = vec![String::new()];
    if root.join(".gitmodules").is_file() {
        repos.extend(parse_submodules(&git(root, &["submodule", "status", "--recursive"]).await?));
    }

    let mut paths = BTreeSet::new();
    for prefix in repos {
        let dir = if prefix.is_empty() { root.to_path_buf() } else { root.join(&prefix) };
        let tracked = match git(&dir, &["diff", "--name-only", "-z", "HEAD"]).await {
            Ok(out) => out,
            // No commit yet: everything in the index is new.
            Err(_) => git(&dir, &["ls-files", "-z", "--cached"]).await?,
        };
        let untracked = git(&dir, &["ls-files", "-z", "--others", "--exclude-standard"]).await?;
        for rel in tracked.split('\0').chain(untracked.split('\0')).filter(|p| !p.is_empty()) {
            let path = if prefix.is_empty() { rel.to_owned() } else { format!("{prefix}/{rel}") };
            // A moved submodule HEAD shows up as its directory; its files are scanned on their own.
            // Brokspace's own state (config, lock files) is not workspace content.
            if !is_brokspace_state(&path) && !root.join(&path).is_dir() {
                paths.insert(path);
            }
        }
    }
    Ok(paths)
}

/// Whether `path` lives in a `.brokspace/` directory (root or submodule).
fn is_brokspace_state(path: &str) -> bool {
    path.starts_with(".brokspace/") || path.contains("/.brokspace/")
}

/// Initialized submodule paths from `git submodule status --recursive`.
pub fn parse_submodules(output: &str) -> Vec<String> {
    output
        .lines()
        .filter(|line| !line.starts_with('-'))
        .filter_map(|line| {
            let (_sha, rest) = line.get(1..)?.trim_start().split_once(' ')?;
            let path = match rest.rfind(" (") {
                Some(idx) if rest.ends_with(')') => &rest[..idx],
                _ => rest,
            };
            Some(path.replace('\\', "/"))
        })
        .collect()
}

async fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .context("cannot run git (is it installed and on PATH?)")?;
    if !output.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn present(n: u64) -> Fingerprint {
        Fingerprint::Present { len: n, modified_ns: u128::from(n) }
    }

    fn snapshot(entries: &[(&str, Fingerprint)]) -> Snapshot {
        entries.iter().map(|(p, f)| ((*p).to_owned(), *f)).collect()
    }

    #[test]
    fn first_scan_reports_everything_dirty() {
        let current = snapshot(&[("a.rs", present(1)), ("gone.rs", Fingerprint::Deleted)]);
        let change = diff(&Snapshot::new(), &current, |_| true);
        assert_eq!(change.changed, vec!["a.rs"]);
        assert_eq!(change.deleted, vec!["gone.rs"]);
    }

    #[test]
    fn unchanged_files_are_quiet() {
        let snap = snapshot(&[("a.rs", present(1))]);
        assert_eq!(diff(&snap, &snap, |_| true), FileChanged::default());
    }

    #[test]
    fn edits_are_detected() {
        let before = snapshot(&[("a.rs", present(1)), ("b.rs", present(2))]);
        let after = snapshot(&[("a.rs", present(3)), ("b.rs", present(2)), ("c.rs", present(4))]);
        assert_eq!(diff(&before, &after, |_| true).changed, vec!["a.rs", "c.rs"]);
    }

    #[test]
    fn files_becoming_clean_are_reported() {
        let before = snapshot(&[("reverted.rs", present(1)), ("committed-delete.rs", Fingerprint::Deleted)]);
        let change = diff(&before, &Snapshot::new(), |p| p == "reverted.rs");
        assert_eq!(change.changed, vec!["reverted.rs"]);
        assert_eq!(change.deleted, vec!["committed-delete.rs"]);
    }

    #[test]
    fn brokspace_state_is_ignored() {
        assert!(super::is_brokspace_state(".brokspace/brok.yml"));
        assert!(super::is_brokspace_state("libs/core/.brokspace/shards/shards.lock"));
        assert!(!super::is_brokspace_state("src/brokspace.rs"));
    }

    #[test]
    fn parses_submodule_status() {
        let out = " 1a2b libs/core (v1)\n+3c4d libs/ui\n-5e6f libs/absent\n";
        assert_eq!(parse_submodules(out), vec!["libs/core", "libs/ui"]);
    }

    #[tokio::test]
    async fn tracks_a_real_repository() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let run = |args: &[&str]| {
            let ok = std::process::Command::new("git").arg("-C").arg(root).args(args).output().unwrap();
            assert!(ok.status.success(), "{}", String::from_utf8_lossy(&ok.stderr));
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(root.join("kept.rs"), "fn a() {}").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);

        let mut tracker = Tracker::new(root);
        assert_eq!(tracker.poll().await.unwrap(), FileChanged::default());

        std::fs::write(root.join("new.rs"), "fn b() {}").unwrap();
        std::fs::remove_file(root.join("kept.rs")).unwrap();
        let change = tracker.poll().await.unwrap();
        assert_eq!(change.changed, vec!["new.rs"]);
        assert_eq!(change.deleted, vec!["kept.rs"]);

        assert_eq!(tracker.poll().await.unwrap(), FileChanged::default());
    }
}
