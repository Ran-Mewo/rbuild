//! Ancestor snapshots for three-way merge.
//!
//! After each successful sync, the client records the agreed-upon manifest for
//! a workspace. The next sync diffs the live local tree and the remote against
//! this snapshot to decide what changed on each side. Snapshots live under the
//! config dir, keyed by workspace id, so they never touch the code tree.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use rbuild_proto::config::GlobalConfig;
use rbuild_proto::proto::ManifestEntry;
use rbuild_proto::scan::Manifest;

fn state_dir() -> Result<PathBuf> {
    Ok(GlobalConfig::config_dir()?.join("state"))
}

fn snapshot_path(workspace_id: &str) -> Result<PathBuf> {
    Ok(state_dir()?.join(format!("{workspace_id}.json")))
}

/// Loads the ancestor manifest for a workspace, or an empty one if this machine
/// has never synced it (which makes the first merge a union).
pub fn load(workspace_id: &str) -> Manifest {
    let path = match snapshot_path(workspace_id) {
        Ok(p) => p,
        Err(_) => return Manifest::new(),
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Manifest::new();
    };
    let entries: Vec<ManifestEntry> = serde_json::from_str(&text).unwrap_or_default();
    entries.into_iter().map(|e| (e.path.clone(), e)).collect()
}

/// Records the agreed manifest after a successful sync round.
pub fn save(workspace_id: &str, manifest: &Manifest) -> Result<()> {
    let dir = state_dir()?;
    std::fs::create_dir_all(&dir)?;
    let entries: Vec<&ManifestEntry> = manifest.values().collect();
    let text = serde_json::to_string(&entries)?;
    std::fs::write(snapshot_path(workspace_id)?, text)
        .with_context(|| format!("writing ancestor snapshot for {workspace_id}"))?;
    Ok(())
}

/// Builds the post-merge ancestor: the set of files both sides will agree on
/// once the plan is applied. Starts from the local manifest, applies local
/// deletes and pulled entries (taken from remote), and drops conflict entries
/// (their resolution is machine-local, so they re-evaluate next round).
pub fn next_ancestor(
    local: &Manifest,
    remote: &Manifest,
    pulled: &[String],
    deleted_local: &[String],
    conflicts: &[String],
) -> Manifest {
    let mut next: BTreeMap<String, ManifestEntry> = local.clone();
    for path in deleted_local {
        next.remove(path);
    }
    for path in pulled {
        if let Some(e) = remote.get(path) {
            next.insert(path.clone(), e.clone());
        }
    }
    for path in conflicts {
        next.remove(path);
    }
    next
}
