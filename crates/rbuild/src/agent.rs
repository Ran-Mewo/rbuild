//! The always-on live-sync agent.
//!
//! Started by an OS login service (see `service.rs`), the agent watches every
//! registered code root and keeps each in continuous two-way sync with the
//! remote — the "immediately uploaded, like mutagen" behavior. One agent covers
//! all roots; roots that share a workspace name sync through one task.
//!
//! It is resilient: a dropped connection retries with backoff, and edits to the
//! config (adding/removing roots) restart the watch set without a relaunch.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use notify_debouncer_full::new_debouncer;
use notify_debouncer_full::notify::RecursiveMode;
use rbuild_proto::config::GlobalConfig;

use crate::connection::Connection;

/// Coalesce a burst of writes (branch switch, formatter run) into one round.
const DEBOUNCE: Duration = Duration::from_millis(150);
/// Reconnect backoff bounds when the remote is unreachable.
const RECONNECT_MIN: Duration = Duration::from_secs(2);
const RECONNECT_MAX: Duration = Duration::from_secs(60);

/// Runs the agent until the process is killed. Supervises one sync task per
/// distinct workspace and restarts the set when the config changes.
pub async fn run() -> Result<()> {
    tracing::info!("rbuild agent starting");
    loop {
        let cfg = match GlobalConfig::load() {
            Ok(c) => c,
            Err(_) => {
                // Not configured yet; wait and re-check rather than exit, so the
                // login service can start before `rbuild init`.
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };

        // Group roots by the workspace they map to: roots sharing a name sync
        // together against one mirror.
        let mut by_ws: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for root in &cfg.roots {
            by_ws.entry(root.workspace_id()).or_default().push(root.path.clone());
        }

        let mut tasks = Vec::new();
        for (workspace_id, roots) in by_ws {
            for root in roots {
                let ws = workspace_id.clone();
                tasks.push(tokio::spawn(async move {
                    sync_root_forever(root, ws).await;
                }));
            }
        }

        if tasks.is_empty() {
            tracing::info!("no code roots registered; waiting for config");
        }

        // Poll the config for *content* changes and reload the watch set when it
        // actually changes. We poll rather than watch the file, because the
        // config dir also holds ancestor snapshots that every sync rewrites and
        // filesystem modify/access events on the config file are too noisy.
        let baseline = config_fingerprint();
        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            if config_fingerprint() != baseline {
                tracing::info!("config changed; reloading watch set");
                break;
            }
        }
        for t in tasks {
            t.abort();
        }
    }
}

/// A cheap fingerprint of the config file's content, used to detect real edits
/// (roots added/removed, remote changed) without reacting to access/mtime noise.
fn config_fingerprint() -> Option<u64> {
    let path = GlobalConfig::path().ok()?;
    let bytes = std::fs::read(path).ok()?;
    Some(seahash(&bytes))
}

/// Tiny non-cryptographic hash; we only need change detection, not security.
fn seahash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 1469598103934665603;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h
}

/// Watches one code root, (re)connecting and live-syncing forever. On a dropped
/// connection it backs off and retries; the local copy is the source of truth,
/// so being offline simply pauses propagation.
async fn sync_root_forever(root: PathBuf, workspace_id: String) {
    let mut backoff = RECONNECT_MIN;
    loop {
        match watch_session(&root, &workspace_id).await {
            Ok(()) => backoff = RECONNECT_MIN, // clean end (config reload abort)
            Err(e) => {
                tracing::warn!(root = %root.display(), error = %e, "sync session ended; retrying");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(RECONNECT_MAX);
            }
        }
    }
}

/// One connected session: initial sync, then sync on every debounced change.
async fn watch_session(root: &PathBuf, workspace_id: &str) -> Result<()> {
    let cfg = GlobalConfig::load().context("config")?;
    let mut conn = Connection::connect_or_deploy(&cfg.remote, workspace_id).await?;

    // Initial reconcile so the mirror matches before watching for changes.
    crate::sync::run(1, workspace_id, root, &mut conn.stdout, &mut conn.stdin).await?;
    tracing::info!(root = %root.display(), "live-syncing");

    // notify calls back on its own thread; forward each batch into an async
    // channel so this task can await changes without blocking a runtime worker.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut debouncer = new_debouncer(DEBOUNCE, None, move |res| {
        let _ = tx.send(res);
    })
    .context("creating file watcher")?;
    debouncer
        .watch(root, RecursiveMode::Recursive)
        .with_context(|| format!("watching {}", root.display()))?;

    while let Some(batch) = rx.recv().await {
        match batch {
            Ok(events) if !events.is_empty() => {
                crate::sync::run(1, workspace_id, root, &mut conn.stdout, &mut conn.stdin).await?;
            }
            Ok(_) => {}
            Err(errors) => {
                for e in errors {
                    tracing::warn!(error = %e, "watch error");
                }
            }
        }
    }
    Ok(())
}

