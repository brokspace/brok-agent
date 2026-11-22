//! `brok-agent`: the default Brokspace system shard.
//!
//! - tracks workspace changes incrementally through git and publishes
//!   `file:changed` (optionally after an LLM review, `review:result`);
//! - answers `agent:ask` with retrieval augmented generation over the local
//!   index, streaming `agent:ask.chunk` replies.
//!
//! It is launched by `brok run`; see the README for running it standalone.

mod ask;
mod context;
mod guardrails;
mod llm;
mod review;
mod settings;
mod tracker;
mod watch;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use brok_sdk::prelude::*;
use tokio::time::MissedTickBehavior;
use tracing_subscriber::EnvFilter;

use crate::review::Reviewer;
use crate::settings::{AgentSettings, LlmConfig};
use crate::tracker::Tracker;
use crate::watch::{FsWatcher, Strategy};

const NAME: &str = "brok-agent";

/// The manifest compiled into the binary; `brok init` reads it through
/// `brok-agent --manifest` to install the agent as a builtin shard.
const MANIFEST: &str = include_str!("../shard.json");

#[tokio::main]
async fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("--manifest") => {
            println!("{MANIFEST}");
            return ExitCode::SUCCESS;
        }
        Some("--version" | "-V") => {
            println!("{NAME} {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        _ => {}
    }

    let subscriber = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_target(false)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")));
    // Under `brok run` every line is already timestamped by the supervisor.
    if std::env::var_os(brok_sdk::env::ENDPOINT).is_some() {
        subscriber.without_time().init();
    } else {
        subscriber.init();
    }

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
    let (client, mut inbox) =
        BrokClient::connect_from_env(NAME, env!("CARGO_PKG_VERSION")).await.context("cannot join the brok bus")?;
    let context = client.context().clone();
    let settings = Arc::new(AgentSettings::from_value(&context.settings)?);
    let llm = Arc::new(LlmConfig::from_value(&context.llm)?);
    client.subscribe([TopicPattern::new(topics::AGENT_ASK)?]).await?;

    let reviewer = Reviewer::new(&settings.review, &llm, context.workspace_root.clone()).unwrap_or_else(|e| {
        tracing::warn!("pre-indexation review disabled: {e:#}");
        None
    });
    let tracking = tokio::spawn(track(client.clone(), context.workspace_root.clone(), Arc::clone(&settings), reviewer));
    tracing::info!(workspace = %context.workspace_root.display(), "brok-agent ready");

    loop {
        tokio::select! {
            event = inbox.recv() => match event {
                None => {
                    tracing::info!("bus connection closed");
                    break;
                }
                Some(event) if event.is::<Shutdown>() => {
                    tracing::info!("shutdown requested");
                    break;
                }
                Some(event) if event.is::<AskRequest>() => {
                    tokio::spawn(ask::handle(client.clone(), event, Arc::clone(&settings), Arc::clone(&llm)));
                }
                Some(event) => tracing::debug!(topic = %event.topic, "ignoring event"),
            },
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    tracking.abort();
    Ok(())
}

/// Where the next scan comes from.
enum Trigger {
    /// A timer: the original behavior.
    Polling(tokio::time::Interval),
    /// File system events, with an optional safety scan on a timer.
    Events { watcher: Box<FsWatcher>, reconcile: Option<tokio::time::Interval> },
}

/// Starts watching, or says why the workspace is polled instead.
async fn start_trigger(root: &Path, settings: &AgentSettings) -> Trigger {
    let polling = || {
        let mut ticker = tokio::time::interval(Duration::from_secs(settings.watch_interval_secs.max(1)));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Trigger::Polling(ticker)
    };
    if settings.watch == Strategy::Polling {
        return polling();
    }
    let dir = root.to_path_buf();
    // Walking the tree is blocking work.
    match tokio::task::spawn_blocking(move || FsWatcher::start(&dir)).await {
        Ok(Ok(watcher)) => {
            tracing::info!(directories = watcher.watched_dirs(), "watching the workspace for changes");
            let reconcile = (settings.watch == Strategy::Auto && settings.reconcile_interval_secs > 0).then(|| {
                let every = Duration::from_secs(settings.reconcile_interval_secs.max(10));
                let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
                ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
                ticker
            });
            Trigger::Events { watcher: Box::new(watcher), reconcile }
        }
        Ok(Err(e)) => {
            tracing::warn!(
                "cannot watch the workspace ({e:#}); polling every {}s instead",
                settings.watch_interval_secs.max(1)
            );
            polling()
        }
        Err(e) => {
            tracing::warn!("cannot watch the workspace ({e}); polling instead");
            polling()
        }
    }
}

/// Detects, reviews and publishes changes until the connection closes.
///
/// The first scan runs at once. After that a scan runs when the file system
/// reports a change (debounced), when events were lost, and, in `auto` mode,
/// on a slow timer that catches anything the OS did not tell us. With
/// `watch: polling` it runs on a timer, as before.
async fn track(client: BrokClient, root: PathBuf, settings: Arc<AgentSettings>, reviewer: Option<Reviewer>) {
    let mut tracker = Tracker::new(root.clone());
    let mut trigger = start_trigger(&root, &settings).await;
    let debounce = Duration::from_millis(settings.debounce_ms.max(50));
    let max_wait = (debounce * 10).max(Duration::from_secs(2));
    let mut last_error: Option<String> = None;
    let mut first = true;
    loop {
        if !first {
            match &mut trigger {
                Trigger::Polling(ticker) => {
                    ticker.tick().await;
                }
                Trigger::Events { watcher, reconcile } => {
                    let reconcile_tick = async {
                        match reconcile {
                            Some(ticker) => {
                                ticker.tick().await;
                            }
                            None => std::future::pending().await,
                        }
                    };
                    tokio::select! {
                        batch = watcher.next_batch(debounce, max_wait) => match batch {
                            Some(batch) => {
                                if batch.rediscover || batch.overflow {
                                    tracker.invalidate_repositories();
                                }
                                if batch.overflow {
                                    tracing::debug!("file system events were lost; scanning everything");
                                }
                            }
                            None => {
                                tracing::warn!("the file watcher stopped; polling instead");
                                let mut ticker = tokio::time::interval(Duration::from_secs(settings.watch_interval_secs.max(1)));
                                ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
                                trigger = Trigger::Polling(ticker);
                                continue;
                            }
                        },
                        () = reconcile_tick => tracing::debug!("reconciliation scan"),
                    }
                }
            }
        }
        first = false;
        let change = match tracker.poll().await {
            Ok(change) => {
                last_error = None;
                change
            }
            Err(e) => {
                let message = format!("{e:#}");
                if last_error.as_ref() != Some(&message) {
                    tracing::warn!("change tracking unavailable: {message}");
                    last_error = Some(message);
                }
                continue;
            }
        };
        // New repositories need their git metadata watched.
        if let Trigger::Events { watcher, .. } = &mut trigger
            && let Err(e) = watcher.sync_repositories(tracker.repositories())
        {
            tracing::warn!("cannot watch git metadata ({e:#}); polling instead");
            let mut ticker = tokio::time::interval(Duration::from_secs(settings.watch_interval_secs.max(1)));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            trigger = Trigger::Polling(ticker);
        }
        if change.changed.is_empty() && change.deleted.is_empty() {
            continue;
        }
        if let Some(reviewer) = &reviewer {
            reviewer.review_all(&client, &change.changed).await;
        }
        tracing::info!(changed = change.changed.len(), deleted = change.deleted.len(), "workspace changed");
        if let Err(e) = client.emit(&change).await {
            tracing::warn!(error = %e, "cannot publish file:changed");
            if matches!(e, ClientError::Closed) {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use brok_sdk::ShardManifest;

    #[test]
    fn shard_manifest_is_valid() {
        let manifest = ShardManifest::from_json(super::MANIFEST).unwrap();
        assert_eq!(manifest.name, super::NAME);
        assert_eq!(manifest.version.to_string(), env!("CARGO_PKG_VERSION"));
        assert!(manifest.permissions.process.iter().any(|p| p == "git"));
    }
}
