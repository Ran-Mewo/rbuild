//! `rbuild download <name> <dir>`: materialize a shared workspace locally.
//!
//! Creates (or reuses) `dir`, registers it as a code root under the given
//! shared workspace name, and runs one sync. Because the local tree and the
//! ancestor snapshot are both empty, the merge pulls the entire remote
//! workspace down — after which `dir` is a normal, two-way-synced code root.

use std::path::PathBuf;

use anyhow::{Context, Result};
use rbuild_proto::config::{workspace_id_for_name, GlobalConfig};

use crate::connection::Connection;
use crate::sync;

pub async fn run(name: String, dir: PathBuf) -> Result<()> {
    let mut cfg = GlobalConfig::load()
        .context("no global config — run `rbuild init <host>` first")?;

    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let root = std::fs::canonicalize(&dir)?;

    // Register the directory under the shared name so future builds/syncs in it
    // address the same remote workspace.
    cfg.add_root(&root, name.clone());
    cfg.save()?;
    crate::shim::install_shims(&cfg.build.commands)?;

    let workspace_id = workspace_id_for_name(&name);
    println!("Downloading workspace '{name}' into {} …", root.display());
    let mut conn = Connection::connect_or_deploy(&cfg.remote, &workspace_id).await?;
    let stats = sync::run(1, &workspace_id, &root, &mut conn.stdout, &mut conn.stdin).await?;
    conn.shutdown().await?;

    println!(
        "Downloaded {} file(s) into {}.",
        stats.pulled,
        root.display()
    );
    if std::env::var_os("RBUILD_SHIM_DIR").is_none() {
        println!(
            "\nTo enable transparent builds, install the shell hook once:\n  \
             rbuild init-shell <bash|zsh|fish|powershell|cmd>"
        );
    }
    Ok(())
}
