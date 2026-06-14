//! `rbuild` — the local client. Your machine is the source of truth; this
//! binary syncs your code roots to the remote, dispatches builds, and installs
//! the shell interception. Configure it once (`init` + `add`), and every
//! project under a registered code root builds remotely.

mod agent;
mod ancestor;
mod build;
mod connection;
mod deploy;
mod download;
mod image;
mod service;
mod shell_hook;
mod shim;
mod sync;

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rbuild_proto::config::{BuildSettings, GlobalConfig, RemoteConfig};
use rbuild_proto::proto::Target;

use connection::Connection;

#[derive(Parser)]
#[command(name = "rbuild", version, about = "Transparent remote build offloading")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Point rbuild at a remote SSH host (writes global config).
    Init {
        /// SSH destination, e.g. `build-host` or `user@1.2.3.4`.
        host: String,
    },
    /// Register a code root (e.g. ~/Code). Everything under it builds remotely.
    Add {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Shared workspace name. Defaults to the folder's name, so same-named
        /// folders on different machines share and merge (e.g. ~/Code and
        /// D:/Code). Override to keep two same-named folders separate, or to set
        /// a custom shared name.
        #[arg(long = "as")]
        name: Option<String>,
    },
    /// List registered code roots.
    Roots,
    /// Pull a remote workspace into a local directory and register it.
    /// e.g. `rbuild download shared ~/Code` recreates a workspace shared from
    /// another machine.
    Download {
        /// The shared workspace name to pull.
        name: String,
        /// Local directory to create/populate and register as a code root.
        dir: PathBuf,
    },
    /// Run a command in the remote build container for the current OS target.
    /// Use this to build, test, or install deps — e.g. `rbuild run pip install -r reqs.txt`.
    Run {
        #[arg(trailing_var_arg = true, required = true)]
        argv: Vec<String>,
    },
    /// Force a Windows (Wine) build/command, regardless of the client OS.
    Win {
        #[arg(trailing_var_arg = true, required = true)]
        argv: Vec<String>,
    },
    /// Force a Linux build/command, regardless of the client OS.
    Linux {
        #[arg(trailing_var_arg = true, required = true)]
        argv: Vec<String>,
    },
    /// Sync the current code root to the remote once and exit.
    Sync {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Verify connectivity: launch the remote daemon and handshake.
    Connect,
    /// Install the shell hook that activates interception inside code roots.
    InitShell {
        /// bash, zsh, fish, powershell, or cmd.
        shell: String,
    },
    /// Remove rbuild entirely: shell hooks, shims, config, and the binary.
    /// Prompts about wiping the daemon on the remote (in case this is a
    /// client-only removal).
    Uninstall {
        /// Skip the interactive prompt; do not touch the remote.
        #[arg(long)]
        keep_remote: bool,
        /// Skip the interactive prompt; do wipe the remote.
        #[arg(long, conflicts_with = "keep_remote")]
        wipe_remote: bool,
    },
    /// Internal: print the PATH adjustment for the current directory. Called by
    /// the installed shell hook on each prompt; not meant to be run directly.
    #[command(hide = true)]
    Hook {
        shell: String,
    },
    /// Internal: the always-on live-sync agent. Started by the login service,
    /// not meant to be run directly.
    #[command(hide = true)]
    Agent,
}

#[tokio::main]
async fn main() -> Result<()> {
    // If invoked under a build-command name (via a shim), run in shim mode
    // before any CLI parsing — argv[0] is the command, not `rbuild`.
    if let Some(command) = shim::invoked_as() {
        let code = shim::run_as_shim(command).await?;
        std::process::exit(code);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().command {
        Cmd::Init { host } => cmd_init(host),
        Cmd::Add { path, name } => cmd_add(path, name),
        Cmd::Roots => cmd_roots(),
        Cmd::Download { name, dir } => cmd_download(name, dir).await,
        Cmd::Run { argv } => exit_with(cmd_build(argv, Target::host()).await?),
        Cmd::Win { argv } => exit_with(cmd_build(argv, Target::Windows).await?),
        Cmd::Linux { argv } => exit_with(cmd_build(argv, Target::Linux).await?),
        Cmd::Sync { path } => cmd_sync(path).await,
        Cmd::Connect => cmd_connect().await,
        Cmd::InitShell { shell } => cmd_init_shell(shell),
        Cmd::Uninstall { keep_remote, wipe_remote } => {
            cmd_uninstall(keep_remote, wipe_remote).await
        }
        Cmd::Hook { shell } => cmd_hook(shell),
        Cmd::Agent => agent::run().await,
    }
}

fn exit_with(code: i32) -> ! {
    std::process::exit(code);
}

fn cmd_init(host: String) -> Result<()> {
    // Preserve roots/build settings if reconfiguring an existing install.
    let mut cfg = GlobalConfig::load().unwrap_or_else(|_| GlobalConfig {
        remote: RemoteConfig {
            host: host.clone(),
            identity_file: None,
            docker: String::new(),
        },
        roots: Vec::new(),
        build: BuildSettings::default(),
    });
    cfg.remote.host = host;
    cfg.save()?;
    println!("Wrote global config to {}", GlobalConfig::path()?.display());
    println!("Next: `rbuild add ~/Code` then `rbuild init-shell <shell>`.");
    Ok(())
}

fn cmd_add(path: PathBuf, name: Option<String>) -> Result<()> {
    let root = std::fs::canonicalize(&path)
        .with_context(|| format!("resolving {}", path.display()))?;
    let mut cfg = GlobalConfig::load()
        .context("no global config — run `rbuild init <host>` first")?;
    let name = name.unwrap_or_else(|| rbuild_proto::config::default_workspace_name(&root));
    cfg.add_root(&root, name.clone());
    cfg.save()?;
    // (Re)install shims for the configured command set; they live in rbuild's
    // own dir, so your code root stays clean.
    shim::install_shims(&cfg.build.commands)?;
    println!("Tracking code root {} as workspace '{}'", root.display(), name);

    // Ensure the always-on live-sync agent is installed and running, so edits
    // propagate continuously without any command — this is the "live sync"
    // behavior, not something the user starts by hand.
    match service::install() {
        Ok(()) => println!("Live-sync agent is running; edits sync automatically."),
        Err(e) => println!(
            "Note: could not start the live-sync agent ({e}). \
             Builds still sync on demand; see `rbuild` docs to start it manually."
        ),
    }

    if std::env::var_os("RBUILD_SHIM_DIR").is_none() {
        println!(
            "\nTo enable transparent builds, install the shell hook once:\n  \
             rbuild init-shell <bash|zsh|fish|powershell|cmd>"
        );
    }
    Ok(())
}

fn cmd_roots() -> Result<()> {
    let cfg = GlobalConfig::load().context("no global config — run `rbuild init <host>` first")?;
    if cfg.roots.is_empty() {
        println!("No code roots registered. Add one with `rbuild add ~/Code`.");
    } else {
        for r in &cfg.roots {
            println!("{}  (workspace '{}')", r.path.display(), r.name);
        }
    }
    Ok(())
}

async fn cmd_download(name: String, dir: PathBuf) -> Result<()> {
    download::run(name, dir).await
}

async fn cmd_connect() -> Result<()> {
    let cfg = GlobalConfig::load().context("no global config — run `rbuild init <host>` first")?;
    println!("Connecting to {} …", cfg.remote.host);
    // A probe workspace just to exercise the launch + handshake path.
    let conn = Connection::connect_or_deploy(&cfg.remote, "probe").await?;
    println!("Connected. Remote rbuildd version {}", conn.daemon_version);
    conn.shutdown().await?;
    Ok(())
}

async fn cmd_sync(path: PathBuf) -> Result<()> {
    let start = std::fs::canonicalize(&path)?;
    let cfg = GlobalConfig::load().context("no global config — run `rbuild init <host>` first")?;
    let loc = cfg
        .locate(&start)
        .context("not inside a registered code root — add one with `rbuild add`")?;

    let mut conn = Connection::connect_or_deploy(&cfg.remote, &loc.workspace_id).await?;
    let t0 = std::time::Instant::now();
    let stats =
        sync::run(1, &loc.workspace_id, &loc.root, &mut conn.stdout, &mut conn.stdin).await?;
    println!(
        "Synced ↑{} ↓{}, deleted {}, {} conflict(s) in {:.2}s",
        stats.sent,
        stats.pulled,
        stats.deleted,
        stats.conflicts,
        t0.elapsed().as_secs_f64()
    );
    let _ = stats.applied;
    conn.shutdown().await?;
    Ok(())
}

/// Syncs the current code root, runs `argv` on the remote in the matching
/// subdirectory, and returns the remote exit code. Shared by the shims,
/// `rbuild run`, and `rbuild win`.
async fn cmd_build(argv: Vec<String>, target: Target) -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let cfg = GlobalConfig::load().context("no global config — run `rbuild init <host>` first")?;
    let loc = cfg
        .locate(&cwd)
        .context("not inside a registered code root — add one with `rbuild add`")?;

    let mut conn = Connection::connect_or_deploy(&cfg.remote, &loc.workspace_id).await?;
    // connect_or_deploy may have detected and cached the docker invocation;
    // reload so image building uses it.
    let cfg = GlobalConfig::load().unwrap_or(cfg);
    // Ensure the build image for this target exists on the remote, building it
    // the first time (over SSH, no host-FS footprint).
    image::ensure_image(&cfg.remote, target)
        .await
        .context("preparing the remote build image")?;
    // Always sync before building so the remote reflects the latest edits.
    sync::run(1, &loc.workspace_id, &loc.root, &mut conn.stdout, &mut conn.stdin).await?;
    let code = build::dispatch(
        &mut conn,
        &cfg,
        &loc.workspace_id,
        &loc.root,
        argv,
        loc.rel_cwd,
        target,
    )
    .await?;
    conn.shutdown().await?;
    Ok(code)
}

fn cmd_init_shell(shell: String) -> Result<()> {
    let sh = shell_hook::Shell::parse(&shell)
        .with_context(|| format!("unknown shell {shell:?} (bash|zsh|fish|powershell|cmd)"))?;
    let exe = std::env::current_exe().context("locating rbuild binary")?;
    shell_hook::install(sh, &exe)?;
    println!(
        "Installed rbuild hook for {}. Restart your shell or open a new one to activate it.",
        sh.name()
    );
    Ok(())
}

fn cmd_hook(shell: String) -> Result<()> {
    let sh = shell_hook::Shell::parse(&shell)
        .with_context(|| format!("unknown shell {shell:?}"))?;
    print!("{}", shell_hook::emit_hook(sh)?);
    Ok(())
}

/// Removes rbuild completely: shell hooks, shims, config dir, and finally the
/// binary itself. Asks whether to also wipe the daemon on the remote, since the
/// user may be removing only this client.
async fn cmd_uninstall(keep_remote: bool, wipe_remote: bool) -> Result<()> {
    // Decide on remote wipe up front (while config still exists).
    let do_remote = if wipe_remote {
        true
    } else if keep_remote {
        false
    } else {
        prompt_yes_no("Also wipe rbuildd and all build state on the remote?")
    };

    if do_remote {
        if let Ok(cfg) = GlobalConfig::load() {
            match wipe_remote_daemon(&cfg).await {
                Ok(()) => println!("Wiped rbuildd state on {}.", cfg.remote.host),
                Err(e) => eprintln!("rbuild: could not wipe remote ({e}); continuing."),
            }
        }
    }

    // Local cleanup: shell hooks, shims, then the whole config dir.
    for sh in [
        shell_hook::Shell::Bash,
        shell_hook::Shell::Zsh,
        shell_hook::Shell::Fish,
        shell_hook::Shell::PowerShell,
        shell_hook::Shell::Cmd,
    ] {
        shell_hook::uninstall(sh).ok();
    }
    shim::remove_shims().ok();
    // Stop and remove the login service running the agent.
    service::uninstall().ok();
    if let Ok(dir) = GlobalConfig::config_dir() {
        std::fs::remove_dir_all(&dir).ok();
    }
    println!("Removed rbuild shell integration, shims, and config.");
    println!("Open a new shell to return to normal.");

    // Finally, delete the binary itself.
    if let Ok(exe) = std::env::current_exe() {
        match remove_self(&exe) {
            Ok(()) => println!("Removed {}.", exe.display()),
            Err(e) => println!(
                "Could not remove the rbuild binary at {} ({e}); delete it manually.",
                exe.display()
            ),
        }
    }
    Ok(())
}

async fn wipe_remote_daemon(cfg: &GlobalConfig) -> Result<()> {
    // Remove every labelled volume and rbuild image — the whole remote
    // footprint. No host paths are involved.
    deploy::wipe_remote(&cfg.remote).await
}

/// Prompts on the terminal; defaults to "no" when not interactive so scripted
/// or piped invocations never accidentally wipe a shared remote.
fn prompt_yes_no(question: &str) -> bool {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return false;
    }
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

#[cfg(unix)]
fn remove_self(exe: &std::path::Path) -> std::io::Result<()> {
    // On Unix the running binary can be unlinked while executing.
    std::fs::remove_file(exe)
}

#[cfg(windows)]
fn remove_self(exe: &std::path::Path) -> std::io::Result<()> {
    // A running .exe can't delete itself directly; schedule deletion via a
    // detached cmd that waits for this process to exit, then deletes the file.
    // The path is quoted inside the command string so paths with spaces work.
    use std::process::Command;
    let cmd = format!("ping 127.0.0.1 -n 2 >nul & del /f /q \"{}\"", exe.display());
    Command::new("cmd").args(["/C", &cmd]).spawn().map(|_| ())
}
