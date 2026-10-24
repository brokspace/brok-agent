//! Incremental change tracking through git.
//!
//! Every tick the tracker asks git, in every repository of the workspace
//! (the root repository, its initialized submodules and independent
//! repositories cloned inside the workspace), two questions:
//!
//! - which files are dirty (modified, deleted or untracked)? They are
//!   fingerprinted and compared with the previous tick;
//! - did `HEAD` move (commit, pull, checkout, rebase)? Then the files that
//!   differ between the old and the new commit changed as well.
//!
//! Only the difference is published, so an idle workspace costs a couple of
//! `git` invocations per repository.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{Context, bail};
use brok_sdk::payload::FileChanged;
use tokio::process::Command;

/// How often nested repositories are rediscovered.
const REDISCOVER_EVERY: Duration = Duration::from_secs(60);
/// How deep below the workspace root nested repositories are searched.
const MAX_REPO_DEPTH: usize = 4;
/// Directories never searched for nested repositories.
const SKIP_DIRS: &[&str] = &["node_modules", "target", "dist", "build", "vendor"];
/// Marker of folders brok generated for other tools (exported skills).
const MANAGED_MARKER: &str = ".brokspace-managed";

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

/// Adds the files touched by moved `HEAD`s to `change`: existing files are
/// changed, missing ones deleted. Paths already reported are left alone.
pub fn merge_committed(change: &mut FileChanged, committed: &BTreeSet<String>, exists: impl Fn(&str) -> bool) {
    for path in committed {
        if change.changed.contains(path) || change.deleted.contains(path) {
            continue;
        }
        if exists(path) { change.changed.push(path.clone()) } else { change.deleted.push(path.clone()) }
    }
    change.changed.sort();
    change.deleted.sort();
}

/// Polls git and reports incremental changes.
#[derive(Debug)]
pub struct Tracker {
    root: PathBuf,
    previous: Snapshot,
    /// Last seen `HEAD` per repository prefix (`""` = workspace root).
    heads: BTreeMap<String, String>,
    repos: Vec<String>,
    discovered_at: Option<Instant>,
}

impl Tracker {
    /// Tracker for the workspace at `root`. The first poll reports every
    /// dirty file, but not the history behind the current commits.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            previous: Snapshot::new(),
            heads: BTreeMap::new(),
            repos: Vec::new(),
            discovered_at: None,
        }
    }

    /// Repository prefixes currently tracked (`""` = workspace root).
    #[cfg(test)]
    pub fn repositories(&self) -> &[String] {
        &self.repos
    }

    /// Scans the workspace and returns what changed since the last poll.
    pub async fn poll(&mut self) -> anyhow::Result<FileChanged> {
        if self.discovered_at.is_none_or(|at| at.elapsed() >= REDISCOVER_EVERY) {
            let repos = discover_repositories(&self.root).await;
            if repos != self.repos {
                tracing::info!(repositories = ?repos, "tracking git repositories");
                self.repos = repos;
            }
            self.discovered_at = Some(Instant::now());
        }

        let mut current = Snapshot::new();
        let mut committed = BTreeSet::new();
        for prefix in self.repos.clone() {
            let dir = if prefix.is_empty() { self.root.clone() } else { self.root.join(&prefix) };
            if let Ok(head) = git(&dir, &["rev-parse", "HEAD"]).await {
                let head = head.trim().to_owned();
                if let Some(old) = self.heads.get(&prefix).filter(|old| **old != head) {
                    match git(&dir, &["diff", "--name-only", "--relative", "-z", old, &head]).await {
                        Ok(names) => committed.extend(
                            names
                                .split('\0')
                                .filter(|p| !p.is_empty())
                                .map(|rel| prefixed(&prefix, rel))
                                .filter(|path| !is_ignored(&self.root, path)),
                        ),
                        Err(e) => tracing::debug!(repo = %prefix, error = %e, "cannot diff a moved HEAD"),
                    }
                }
                self.heads.insert(prefix.clone(), head);
            }
            for path in dirty_paths(&self.root, &dir, &prefix).await? {
                current.insert(path.clone(), fingerprint(&self.root.join(&path)));
            }
        }

        let root = self.root.clone();
        let exists = |p: &str| root.join(p).is_file();
        let mut change = diff(&self.previous, &current, exists);
        merge_committed(&mut change, &committed, exists);
        self.previous = current;
        Ok(change)
    }
}

fn prefixed(prefix: &str, rel: &str) -> String {
    if prefix.is_empty() { rel.to_owned() } else { format!("{prefix}/{rel}") }
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

/// Every repository of the workspace, as prefixes relative to `root`: the
/// root itself when it is inside a git work tree, its initialized
/// submodules, and independent repositories nested inside it.
async fn discover_repositories(root: &Path) -> Vec<String> {
    let mut repos = BTreeSet::new();
    let root_is_repo = git(root, &["rev-parse", "--is-inside-work-tree"]).await.is_ok_and(|out| out.trim() == "true");
    if root_is_repo {
        repos.insert(String::new());
        if root.join(".gitmodules").is_file() {
            match git(root, &["submodule", "status", "--recursive"]).await {
                Ok(out) => repos.extend(parse_submodules(&out)),
                Err(e) => tracing::warn!(error = %e, "cannot list submodules"),
            }
        }
    }
    let scan_root = root.to_path_buf();
    match tokio::task::spawn_blocking(move || find_nested_repositories(&scan_root, MAX_REPO_DEPTH)).await {
        Ok(nested) => repos.extend(nested),
        Err(e) => tracing::warn!(error = %e, "cannot scan for nested repositories"),
    }
    if repos.is_empty() {
        tracing::warn!(root = %root.display(), "no git repository found; changes are not tracked");
    }
    repos.into_iter().collect()
}

/// Directories below `root` (up to `max_depth` levels) that hold a `.git`
/// directory or file, skipping hidden folders and build/dependency output.
pub fn find_nested_repositories(root: &Path, max_depth: usize) -> Vec<String> {
    fn visit(dir: &Path, rel: &str, depth: usize, max_depth: usize, out: &mut Vec<String>) {
        if depth > max_depth {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut children: Vec<(String, PathBuf)> = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| Some((e.file_name().to_str()?.to_owned(), e.path())))
            .filter(|(name, _)| !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_str()))
            .collect();
        children.sort();
        for (name, path) in children {
            let child_rel = prefixed(rel, &name);
            if path.join(".git").exists() {
                out.push(child_rel.clone());
            }
            visit(&path, &child_rel, depth + 1, max_depth, out);
        }
    }
    let mut out = Vec::new();
    visit(root, "", 1, max_depth, &mut out);
    out
}

/// Dirty files of one repository, as workspace relative paths.
async fn dirty_paths(root: &Path, dir: &Path, prefix: &str) -> anyhow::Result<BTreeSet<String>> {
    let tracked = match git(dir, &["diff", "--name-only", "--relative", "-z", "HEAD"]).await {
        Ok(out) => out,
        // No commit yet: everything in the index is new.
        Err(_) => git(dir, &["ls-files", "-z", "--cached"]).await?,
    };
    let untracked = git(dir, &["ls-files", "-z", "--others", "--exclude-standard"]).await?;
    let mut paths = BTreeSet::new();
    for rel in tracked.split('\0').chain(untracked.split('\0')).filter(|p| !p.is_empty()) {
        let path = prefixed(prefix, rel);
        // Submodules and nested repositories show up as their directory;
        // their files are scanned on their own.
        if !is_ignored(root, &path) && !root.join(&path).is_dir() {
            paths.insert(path);
        }
    }
    Ok(paths)
}

/// Brokspace's own state and folders brok generated are not workspace content.
fn is_ignored(root: &Path, path: &str) -> bool {
    is_brokspace_state(path) || in_managed_dir(root, path)
}

/// Whether `path` lives in a `.brokspace/` directory (root or submodule).
fn is_brokspace_state(path: &str) -> bool {
    path.starts_with(".brokspace/") || path.contains("/.brokspace/")
}

/// Whether `path` lies in a folder holding a `.brokspace-managed` marker.
fn in_managed_dir(root: &Path, path: &str) -> bool {
    let mut dir = root.to_path_buf();
    let parts: Vec<&str> = path.split('/').collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        dir.push(part);
        if dir.join(MANAGED_MARKER).is_file() {
            return true;
        }
    }
    false
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
    fn committed_files_are_merged() {
        let mut change = FileChanged { changed: vec!["dirty.rs".into()], deleted: vec![] };
        let committed: BTreeSet<String> = ["dirty.rs", "new.rs", "gone.rs"].map(String::from).into();
        merge_committed(&mut change, &committed, |p| p != "gone.rs");
        assert_eq!(change.changed, vec!["dirty.rs", "new.rs"]);
        assert_eq!(change.deleted, vec!["gone.rs"]);
    }

    #[test]
    fn brokspace_state_is_ignored() {
        assert!(super::is_brokspace_state(".brokspace/brok.yml"));
        assert!(super::is_brokspace_state("libs/core/.brokspace/shards/shards.lock"));
        assert!(!super::is_brokspace_state("src/brokspace.rs"));
    }

    #[test]
    fn managed_folders_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude/skills/tdd")).unwrap();
        std::fs::write(dir.path().join(".claude/skills/tdd/.brokspace-managed"), "").unwrap();
        assert!(is_ignored(dir.path(), ".claude/skills/tdd/SKILL.md"));
        assert!(!is_ignored(dir.path(), "src/main.rs"));
    }

    #[test]
    fn parses_submodule_status() {
        let out = " 1a2b libs/core (v1)\n+3c4d libs/ui\n-5e6f libs/absent\n";
        assert_eq!(parse_submodules(out), vec!["libs/core", "libs/ui"]);
    }

    #[test]
    fn finds_nested_repositories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for repo in ["api/.git", "libs/ui/.git", "node_modules/pkg/.git", ".cache/x/.git", "a/b/c/d/e/.git"] {
            std::fs::create_dir_all(root.join(repo)).unwrap();
        }
        std::fs::create_dir_all(root.join("tools")).unwrap();
        std::fs::write(root.join("tools/.git"), "gitdir: ../.git/modules/tools").unwrap();
        assert_eq!(find_nested_repositories(root, MAX_REPO_DEPTH), vec!["api", "libs/ui", "tools"]);
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git_in(dir, &["init", "-q"]);
        git_in(dir, &["config", "user.email", "t@example.com"]);
        git_in(dir, &["config", "user.name", "t"]);
        git_in(dir, &["config", "commit.gpgsign", "false"]);
    }

    fn commit_all(dir: &Path, message: &str) {
        git_in(dir, &["add", "-A"]);
        git_in(dir, &["commit", "-q", "-m", message]);
    }

    #[tokio::test]
    async fn tracks_a_real_repository() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        std::fs::write(root.join("kept.rs"), "fn a() {}").unwrap();
        commit_all(root, "init");

        let mut tracker = Tracker::new(root);
        assert_eq!(tracker.poll().await.unwrap(), FileChanged::default());

        std::fs::write(root.join("new.rs"), "fn b() {}").unwrap();
        std::fs::remove_file(root.join("kept.rs")).unwrap();
        let change = tracker.poll().await.unwrap();
        assert_eq!(change.changed, vec!["new.rs"]);
        assert_eq!(change.deleted, vec!["kept.rs"]);

        assert_eq!(tracker.poll().await.unwrap(), FileChanged::default());
    }

    #[tokio::test]
    async fn commits_and_checkouts_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_repo(root);
        std::fs::write(root.join("kept.rs"), "fn a() {}").unwrap();
        commit_all(root, "init");

        let mut tracker = Tracker::new(root);
        assert_eq!(tracker.poll().await.unwrap(), FileChanged::default(), "history is not replayed");

        git_in(root, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(root.join("feature.rs"), "fn f() {}").unwrap();
        std::fs::remove_file(root.join("kept.rs")).unwrap();
        commit_all(root, "feature");
        // The working tree is clean, yet the commit changed files.
        let change = tracker.poll().await.unwrap();
        assert_eq!(change.changed, vec!["feature.rs"]);
        assert_eq!(change.deleted, vec!["kept.rs"]);

        git_in(root, &["checkout", "-q", "-"]);
        let change = tracker.poll().await.unwrap();
        assert_eq!(change.changed, vec!["kept.rs"]);
        assert_eq!(change.deleted, vec!["feature.rs"]);
        assert_eq!(tracker.poll().await.unwrap(), FileChanged::default());
    }

    #[tokio::test]
    async fn plain_folder_of_repositories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for name in ["api", "web"] {
            let repo = root.join(name);
            init_repo(&repo);
            std::fs::write(repo.join("main.rs"), "fn main() {}").unwrap();
            commit_all(&repo, "init");
        }
        let mut tracker = Tracker::new(root);
        assert_eq!(tracker.poll().await.unwrap(), FileChanged::default());
        assert_eq!(tracker.repositories(), ["api", "web"]);

        std::fs::write(root.join("web/main.rs"), "fn main() { println!(); }").unwrap();
        assert_eq!(tracker.poll().await.unwrap().changed, vec!["web/main.rs"]);
    }
}
