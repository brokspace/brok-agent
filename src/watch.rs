//! Event-driven change detection.
//!
//! Polling asks git about every repository every few seconds, even when
//! nothing happens. This module listens to the operating system instead
//! (inotify, FSEvents, ReadDirectoryChangesW through `notify`) and only wakes
//! the tracker when something that can matter changed. git stays the source
//! of truth: an event is a hint that makes the tracker run its usual scan, so
//! ignore rules, renames, submodules and moved `HEAD`s keep their meaning, and
//! a lost event costs at most one reconciliation interval.
//!
//! Design choices, each one because of a failure mode:
//!
//! - **One watch per directory, found with `ignore`**, never a recursive
//!   watch of the root: that would also watch `node_modules` or `target`
//!   (hundreds of thousands of directories) and run into the inotify limit.
//!   Ignored, hidden-by-convention and build directories are not watched.
//! - **Git metadata is watched on its own** (`HEAD`, `packed-refs`, `refs/`),
//!   resolving the `.git` file of submodules and worktrees, so a commit, pull,
//!   checkout or rebase is noticed even though no source file changed.
//! - **The queue is bounded.** When it is full, or the OS says it lost events,
//!   the watcher reports an overflow and the caller reconciles with a full
//!   scan; memory never grows with a burst.
//! - **Bursts are coalesced** (a `git checkout` or a build touches thousands of
//!   files): one batch after a quiet period, or after `max_wait` at the latest.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, bail};
use notify::event::{CreateKind, EventKind};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// Directories watched at most; beyond this the caller falls back to polling.
pub const MAX_WATCHED_DIRS: usize = 20_000;
/// Hints buffered between the OS thread and the async loop.
const QUEUE: usize = 1024;
/// Directories never watched (build and dependency output).
const SKIP_DIRS: &[&str] = &["node_modules", "target", "dist", "build", "vendor"];

/// How the workspace is watched (`watch` in the agent settings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    /// Native events plus a periodic reconciliation scan; polling when the
    /// workspace cannot be watched.
    #[default]
    Auto,
    /// Native events only (the scan runs at start and after lost events);
    /// polling when the workspace cannot be watched.
    Native,
    /// Scan on a timer, as before.
    Polling,
}

/// What one notification means for the tracker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hint {
    /// Something that can matter changed: scan.
    Changed,
    /// A repository may have appeared or disappeared: discover again.
    Rediscover,
    /// A directory appeared: watch it too.
    NewDir(PathBuf),
    /// Events were lost: scan everything.
    Overflow,
}

/// What a burst of hints adds up to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Batch {
    /// Repositories must be discovered again.
    pub rediscover: bool,
    /// Events were lost (or the queue was full).
    pub overflow: bool,
    /// Hints received, after filtering.
    pub events: usize,
}

/// Turns a raw notification into hints. Pure, so it can be tested without a
/// file system. `git_dirs` are the git directories being watched, which may
/// live outside `root` (worktrees).
pub fn classify(root: &Path, git_dirs: &[PathBuf], event: &Event) -> Vec<Hint> {
    if event.need_rescan() {
        return vec![Hint::Overflow];
    }
    if matches!(event.kind, EventKind::Access(_)) {
        return Vec::new();
    }
    let mut hints = Vec::new();
    for path in &event.paths {
        if let Some(git_dir) = git_dirs.iter().find(|dir| path.starts_with(dir)) {
            let rest = path.strip_prefix(git_dir).unwrap_or(path);
            if git_metadata_matters(rest) {
                hints.push(Hint::Changed);
            }
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else { continue };
        let parts: Vec<&str> = rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(name) => name.to_str(),
                _ => None,
            })
            .collect();
        let Some(name) = parts.last() else { continue };
        // A `.git` anywhere below the root that is not one we watch is the
        // metadata of a repository we do not know yet, or of a submodule.
        if let Some(at) = parts.iter().position(|p| *p == ".git") {
            if at + 1 == parts.len() {
                hints.push(Hint::Rediscover);
                hints.push(Hint::Changed);
            } else if git_metadata_matters(&parts[at + 1..].iter().collect::<PathBuf>()) {
                hints.push(Hint::Changed);
            }
            continue;
        }
        if parts.iter().any(|p| *p == ".brokspace" || SKIP_DIRS.contains(p)) {
            continue;
        }
        if *name == ".gitmodules" {
            hints.push(Hint::Rediscover);
        }
        let is_dir_event = matches!(event.kind, EventKind::Create(CreateKind::Folder))
            || (matches!(event.kind, EventKind::Create(_)) && path.is_dir());
        if is_dir_event {
            hints.push(Hint::NewDir(path.clone()));
        }
        hints.push(Hint::Changed);
    }
    hints
}

/// Whether a path inside a git directory (relative to it) moves `HEAD` or a
/// branch: `HEAD`, `ORIG_HEAD`, `MERGE_HEAD`, `packed-refs`, anything under
/// `refs/`. Objects, the index, reflogs and lock files are noise (git itself
/// writes them when we run `git diff`).
pub fn git_metadata_matters(rel: &Path) -> bool {
    let parts: Vec<&str> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect();
    let Some(name) = parts.last() else { return false };
    if name.ends_with(".lock") || parts.iter().any(|p| matches!(*p, "objects" | "logs" | "hooks" | "info" | "lfs")) {
        return false;
    }
    // Submodule metadata lives under `.git/modules/<name>/`.
    let parts = match parts.iter().position(|p| *p == "modules") {
        Some(at) if at == 0 => &parts[(at + 2).min(parts.len())..],
        _ => &parts[..],
    };
    match parts.first() {
        Some(first) if parts.len() == 1 => matches!(*first, "HEAD" | "ORIG_HEAD" | "MERGE_HEAD" | "packed-refs"),
        Some(first) => *first == "refs",
        None => false,
    }
}

/// The git directory of the repository at `dir`: `.git` itself, or the target
/// of a `.git` file (submodules and linked worktrees).
pub fn resolve_git_dir(dir: &Path) -> Option<PathBuf> {
    let dot_git = dir.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let target = text.lines().find_map(|line| line.strip_prefix("gitdir:"))?.trim();
    let target = Path::new(target);
    let path = if target.is_absolute() { target.to_path_buf() } else { dir.join(target) };
    path.is_dir().then_some(path)
}

/// Pushes `hints` into a bounded queue; a full queue sets `overflow`
/// instead of growing.
fn forward(tx: &mpsc::Sender<Hint>, overflow: &AtomicBool, hints: Vec<Hint>) {
    for hint in hints {
        if tx.try_send(hint).is_err() {
            overflow.store(true, Ordering::Relaxed);
        }
    }
}

/// Collects hints until the burst is over: waits for the first one, then for
/// `debounce` without a new one, but never more than `max_wait` in total.
/// Returns `None` when the sender is gone.
pub async fn collect_burst(rx: &mut mpsc::Receiver<Hint>, debounce: Duration, max_wait: Duration) -> Option<Vec<Hint>> {
    let first = rx.recv().await?;
    let deadline = Instant::now() + max_wait;
    let mut hints = vec![first];
    loop {
        let left = deadline.saturating_duration_since(Instant::now()).min(debounce);
        if left.is_zero() {
            break;
        }
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Some(hint)) => hints.push(hint),
            Ok(None) | Err(_) => break,
        }
    }
    Some(hints)
}

/// Watches a workspace and reports bursts of relevant changes.
pub struct FsWatcher {
    root: PathBuf,
    watcher: RecommendedWatcher,
    rx: mpsc::Receiver<Hint>,
    overflow: Arc<AtomicBool>,
    git_dirs: Arc<RwLock<Vec<PathBuf>>>,
    watched: HashSet<PathBuf>,
}

impl FsWatcher {
    /// Starts watching `root` (every directory that is not ignored or build
    /// output). Fails when the system cannot watch that many directories.
    pub fn start(root: &Path) -> anyhow::Result<Self> {
        let root = root.to_path_buf();
        let (tx, rx) = mpsc::channel(QUEUE);
        let overflow = Arc::new(AtomicBool::new(false));
        let git_dirs: Arc<RwLock<Vec<PathBuf>>> = Arc::new(RwLock::new(Vec::new()));
        let handler = {
            let (root, overflow, git_dirs) = (root.clone(), Arc::clone(&overflow), Arc::clone(&git_dirs));
            move |result: notify::Result<Event>| match result {
                Ok(event) => {
                    let dirs = git_dirs.read().map(|d| d.clone()).unwrap_or_default();
                    forward(&tx, &overflow, classify(&root, &dirs, &event));
                }
                // The backend reports lost events as errors on some platforms.
                Err(_) => overflow.store(true, Ordering::Relaxed),
            }
        };
        let watcher = notify::recommended_watcher(handler).context("cannot start the file watcher")?;
        let mut this = Self { root, watcher, rx, overflow, git_dirs, watched: HashSet::new() };
        let root = this.root.clone();
        this.watch_tree(&root)?;
        Ok(this)
    }

    /// Directories currently watched.
    pub fn watched_dirs(&self) -> usize {
        self.watched.len()
    }

    /// Watches the git metadata of `repos` (prefixes relative to the root,
    /// `""` is the root repository). Safe to call again with a longer list.
    pub fn sync_repositories(&mut self, repos: &[String]) -> anyhow::Result<()> {
        let mut dirs = Vec::new();
        for prefix in repos {
            let dir = if prefix.is_empty() { self.root.clone() } else { self.root.join(prefix) };
            let Some(git_dir) = resolve_git_dir(&dir) else { continue };
            dirs.push(git_dir.clone());
            if self.watched.insert(git_dir.clone()) {
                self.watcher
                    .watch(&git_dir, RecursiveMode::NonRecursive)
                    .with_context(|| format!("watch {}", git_dir.display()))?;
            }
            let refs = git_dir.join("refs");
            if refs.is_dir() && self.watched.insert(refs.clone()) {
                // Branches nest (`refs/heads/feature/x`): refs is small, watch it whole.
                self.watcher
                    .watch(&refs, RecursiveMode::Recursive)
                    .with_context(|| format!("watch {}", refs.display()))?;
            }
        }
        if let Ok(mut known) = self.git_dirs.write() {
            *known = dirs;
        }
        Ok(())
    }

    /// Watches `dir` and every directory below it that is not ignored.
    fn watch_tree(&mut self, dir: &Path) -> anyhow::Result<()> {
        let walker = ignore::WalkBuilder::new(dir)
            .hidden(false)
            .follow_links(false)
            .git_global(false)
            .require_git(false)
            .filter_entry(|entry| {
                entry.file_type().is_some_and(|t| t.is_dir())
                    && entry
                        .file_name()
                        .to_str()
                        .is_none_or(|name| name != ".git" && name != ".brokspace" && !SKIP_DIRS.contains(&name))
            })
            .build();
        for entry in walker.flatten() {
            let path = entry.into_path();
            if self.watched.contains(&path) {
                continue;
            }
            if self.watched.len() >= MAX_WATCHED_DIRS {
                bail!("more than {MAX_WATCHED_DIRS} directories to watch");
            }
            // A directory deleted since the walk is not an error.
            match self.watcher.watch(&path, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    self.watched.insert(path);
                }
                Err(e) if matches!(e.kind, notify::ErrorKind::PathNotFound) => {}
                Err(e) => return Err(e).with_context(|| format!("watch {}", path.display())),
            }
        }
        Ok(())
    }

    /// Waits for the next burst of relevant changes. `None` means the watcher
    /// stopped delivering events for good; the caller should poll.
    pub async fn next_batch(&mut self, debounce: Duration, max_wait: Duration) -> Option<Batch> {
        let hints = collect_burst(&mut self.rx, debounce, max_wait).await?;
        let mut batch = Batch::default();
        for hint in hints {
            batch.events += 1;
            match hint {
                Hint::Changed => {}
                Hint::Rediscover => batch.rediscover = true,
                Hint::Overflow => batch.overflow = true,
                Hint::NewDir(dir) => {
                    // A new directory may hold a repository, and its files must be watched.
                    if let Err(e) = self.watch_tree(&dir) {
                        tracing::warn!(error = %e, "cannot watch a new directory; scanning instead");
                        batch.overflow = true;
                    }
                }
            }
        }
        // Drain what piled up while the queue was full.
        batch.overflow |= self.overflow.swap(false, Ordering::Relaxed);
        Some(batch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, DataChange, ModifyKind, RemoveKind};
    use std::process::Command;

    fn event(kind: EventKind, paths: &[&Path]) -> Event {
        Event { kind, paths: paths.iter().map(|p| p.to_path_buf()).collect(), attrs: Default::default() }
    }

    fn modify() -> EventKind {
        EventKind::Modify(ModifyKind::Data(DataChange::Content))
    }

    #[test]
    fn source_changes_matter_and_noise_does_not() {
        let root = Path::new("/ws");
        let git = [PathBuf::from("/ws/.git")];
        let c = |kind: EventKind, p: &str| classify(root, &git, &event(kind, &[Path::new(p)]));
        assert_eq!(c(modify(), "/ws/src/main.rs"), [Hint::Changed]);
        assert_eq!(c(EventKind::Remove(RemoveKind::File), "/ws/src/old.rs"), [Hint::Changed]);
        assert!(c(modify(), "/ws/node_modules/x/index.js").is_empty());
        assert!(c(modify(), "/ws/target/debug/app").is_empty());
        assert!(c(modify(), "/ws/.brokspace/databases/x").is_empty());
        assert!(c(EventKind::Access(AccessKind::Any), "/ws/src/main.rs").is_empty());
        assert!(c(modify(), "/elsewhere/file").is_empty());
    }

    #[test]
    fn only_head_and_refs_matter_inside_git() {
        let root = Path::new("/ws");
        let git = [PathBuf::from("/ws/.git"), PathBuf::from("/wt/.git/worktrees/w")];
        let c = |p: &str| classify(root, &git, &event(modify(), &[Path::new(p)]));
        assert_eq!(c("/ws/.git/HEAD"), [Hint::Changed]);
        assert_eq!(c("/ws/.git/refs/heads/feature/x"), [Hint::Changed]);
        assert_eq!(c("/ws/.git/packed-refs"), [Hint::Changed]);
        assert_eq!(c("/wt/.git/worktrees/w/HEAD"), [Hint::Changed], "a git dir outside the root");
        assert!(c("/ws/.git/index").is_empty());
        assert!(c("/ws/.git/HEAD.lock").is_empty());
        assert!(c("/ws/.git/objects/ab/cdef").is_empty());
        assert!(c("/ws/.git/logs/HEAD").is_empty());
        assert!(c("/ws/.git/refs/heads/main.lock").is_empty());
    }

    #[test]
    fn submodule_metadata_is_found_under_modules() {
        assert!(git_metadata_matters(Path::new("modules/lib/HEAD")));
        assert!(git_metadata_matters(Path::new("modules/lib/refs/heads/main")));
        assert!(!git_metadata_matters(Path::new("modules/lib/index")));
        assert!(!git_metadata_matters(Path::new("modules/lib/objects/aa/bb")));
    }

    #[test]
    fn new_directories_and_repositories_are_reported() {
        let root = Path::new("/ws");
        let hints =
            classify(root, &[], &event(EventKind::Create(CreateKind::Folder), &[Path::new("/ws/services/new")]));
        assert_eq!(hints, [Hint::NewDir(PathBuf::from("/ws/services/new")), Hint::Changed]);
        let hints = classify(root, &[], &event(EventKind::Create(CreateKind::Folder), &[Path::new("/ws/clone/.git")]));
        assert!(hints.contains(&Hint::Rediscover));
        let hints = classify(root, &[], &event(modify(), &[Path::new("/ws/.gitmodules")]));
        assert!(hints.contains(&Hint::Rediscover));
    }

    #[test]
    fn lost_events_mean_scan_everything() {
        let mut ev = event(EventKind::Other, &[]);
        ev.attrs.set_flag(notify::event::Flag::Rescan);
        assert_eq!(classify(Path::new("/ws"), &[], &ev), [Hint::Overflow]);
    }

    #[test]
    fn git_file_of_a_submodule_is_resolved() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("lib");
        std::fs::create_dir_all(tmp.path().join(".git/modules/lib")).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(".git"), "gitdir: ../.git/modules/lib\n").unwrap();
        assert_eq!(resolve_git_dir(&sub).unwrap(), sub.join("../.git/modules/lib"));
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        assert_eq!(resolve_git_dir(tmp.path()).unwrap(), tmp.path().join(".git"));
        assert!(resolve_git_dir(&tmp.path().join("nothing")).is_none());
    }

    #[test]
    fn a_full_queue_overflows_instead_of_growing() {
        let (tx, mut rx) = mpsc::channel(2);
        let overflow = AtomicBool::new(false);
        forward(&tx, &overflow, vec![Hint::Changed; 5]);
        assert!(overflow.load(Ordering::Relaxed));
        let mut received = 0;
        while rx.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, 2, "the queue never holds more than its capacity");
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_becomes_one_batch_after_the_quiet_period() {
        let (tx, mut rx) = mpsc::channel(64);
        let producer = tokio::spawn(async move {
            for _ in 0..10 {
                tx.send(Hint::Changed).await.unwrap();
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        let burst = collect_burst(&mut rx, Duration::from_millis(300), Duration::from_secs(10)).await.unwrap();
        assert_eq!(burst.len(), 10, "events 100 ms apart are one burst for a 300 ms debounce");
        producer.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_constant_stream_is_cut_at_max_wait() {
        let (tx, mut rx) = mpsc::channel(4096);
        tokio::spawn(async move {
            loop {
                if tx.send(Hint::Changed).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
        let started = Instant::now();
        let burst = collect_burst(&mut rx, Duration::from_millis(300), Duration::from_secs(2)).await.unwrap();
        assert!(started.elapsed() <= Duration::from_millis(2100), "{:?}", started.elapsed());
        assert!(burst.len() > 10);
    }

    #[tokio::test(start_paused = true)]
    async fn a_closed_queue_ends_the_wait() {
        let (tx, mut rx) = mpsc::channel::<Hint>(1);
        drop(tx);
        assert!(collect_burst(&mut rx, Duration::from_millis(10), Duration::from_secs(1)).await.is_none());
    }

    // The tests below use the real operating system watcher and real git.
    const WAIT: Duration = Duration::from_secs(15);

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        // Canonical: macOS temp dirs are symlinks and events carry the real path.
        let root = tmp.path().canonicalize().unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn a() {}\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "initial"]);
        tmp
    }

    async fn watcher_for(tmp: &tempfile::TempDir) -> FsWatcher {
        let root = tmp.path().canonicalize().unwrap();
        let mut w = FsWatcher::start(&root).unwrap();
        w.sync_repositories(&[String::new()]).unwrap();
        // Let the backend settle, and drop what it reports about the setup
        // (macOS delivers file events with a delay).
        tokio::time::sleep(Duration::from_millis(300)).await;
        for _ in 0..8 {
            let quiet = tokio::time::timeout(
                Duration::from_millis(1200),
                w.next_batch(Duration::from_millis(50), Duration::from_millis(400)),
            )
            .await;
            if quiet.is_err() {
                break;
            }
        }
        w
    }

    #[tokio::test]
    async fn editing_a_file_wakes_the_watcher() {
        let tmp = repo();
        let mut w = watcher_for(&tmp).await;
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn b() {}\n").unwrap();
        let batch = tokio::time::timeout(WAIT, w.next_batch(Duration::from_millis(100), Duration::from_secs(2)))
            .await
            .unwrap()
            .unwrap();
        assert!(batch.events > 0);
        assert!(!batch.overflow);
    }

    #[tokio::test]
    async fn an_idle_workspace_produces_no_events() {
        let tmp = repo();
        let mut w = watcher_for(&tmp).await;
        let quiet = tokio::time::timeout(
            Duration::from_millis(1500),
            w.next_batch(Duration::from_millis(100), Duration::from_secs(1)),
        )
        .await;
        assert!(quiet.is_err(), "nothing happened, so nothing may wake the tracker: {quiet:?}");
    }

    #[tokio::test]
    async fn build_output_and_dependencies_are_not_watched() {
        let tmp = repo();
        let root = tmp.path().canonicalize().unwrap();
        for dir in ["node_modules/pkg/deep", "target/debug", ".brokspace/databases"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join(".gitignore"), "ignored_dir/\n").unwrap();
        std::fs::create_dir_all(root.join("ignored_dir/inner")).unwrap();
        let mut w = watcher_for(&tmp).await;
        // Per-directory watches make "ignored directories are not watched" a
        // guarantee on Linux; macOS reports events for the whole tree and relies
        // on the classifier, which drops build output on every platform.
        #[cfg(target_os = "linux")]
        {
            let watched = w.watched_dirs();
            // root, src, and the git metadata: a handful, not the 8 directories created above.
            assert!(watched <= 6, "{watched} directories watched");
            std::fs::write(root.join("ignored_dir/inner/y"), "y").unwrap();
        }
        std::fs::write(root.join("node_modules/pkg/deep/x.js"), "x").unwrap();
        std::fs::write(root.join("target/debug/app"), "x").unwrap();
        std::fs::write(root.join(".brokspace/databases/z"), "z").unwrap();
        let quiet = tokio::time::timeout(
            Duration::from_millis(1500),
            w.next_batch(Duration::from_millis(100), Duration::from_secs(1)),
        )
        .await;
        assert!(quiet.is_err(), "ignored paths must not wake the tracker: {quiet:?}");
    }

    #[tokio::test]
    async fn a_commit_is_noticed_through_the_git_metadata() {
        let tmp = repo();
        let mut w = watcher_for(&tmp).await;
        let root = tmp.path().canonicalize().unwrap();
        git(&root, &["commit", "-q", "--allow-empty", "-m", "empty"]);
        let batch = tokio::time::timeout(WAIT, w.next_batch(Duration::from_millis(100), Duration::from_secs(2)))
            .await
            .unwrap()
            .unwrap();
        assert!(batch.events > 0, "{batch:?}");
    }

    #[tokio::test]
    async fn files_in_a_new_directory_are_watched() {
        let tmp = repo();
        let mut w = watcher_for(&tmp).await;
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/new_module")).unwrap();
        tokio::time::timeout(WAIT, w.next_batch(Duration::from_millis(100), Duration::from_secs(2)))
            .await
            .unwrap()
            .unwrap();
        // Let the new watch settle, then write inside it.
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::write(root.join("src/new_module/mod.rs"), "pub fn n() {}\n").unwrap();
        let batch = tokio::time::timeout(WAIT, w.next_batch(Duration::from_millis(100), Duration::from_secs(2)))
            .await
            .unwrap()
            .unwrap();
        assert!(batch.events > 0);
    }
}
