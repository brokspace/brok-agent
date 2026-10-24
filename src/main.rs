//! `brok-agent`: the default Brokspace system shard.
//!
//! - tracks workspace changes incrementally through git and publishes
//!   `file:changed` (optionally after an LLM review, `review:result`);
//! - answers `agent:ask` with retrieval augmented generation over the local
//!   index, streaming `agent:ask.chunk` replies.
//!
//! It is launched by `brok run`; see the README for running it standalone.

mod ask;
mod guardrails;
mod llm;
mod review;
mod settings;
mod tracker;

use std::io::IsTerminal;
use std::path::PathBuf;
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
    let interval = Duration::from_secs(settings.watch_interval_secs.max(1));
    let tracking = tokio::spawn(track(client.clone(), context.workspace_root.clone(), interval, reviewer));
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

/// Polls git, reviews and publishes changes until the connection closes.
async fn track(client: BrokClient, root: PathBuf, interval: Duration, reviewer: Option<Reviewer>) {
    let mut tracker = Tracker::new(root);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_error: Option<String> = None;
    loop {
        ticker.tick().await;
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
