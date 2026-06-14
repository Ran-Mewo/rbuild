//! Build-command interception.
//!
//! rbuild installs small shims named after build commands (`cargo`, `cmake`,
//! …) into a single directory under its config dir. Each shim is the `rbuild`
//! binary itself invoked under that name; on startup rbuild inspects `argv[0]`
//! and, if it isn't `rbuild`, runs in shim mode.
//!
//! A shim decides at runtime whether to offload or step aside:
//!   * inside a registered code root, and the command is one we intercept
//!     → run it remotely;
//!   * otherwise → exec the real tool, unchanged.
//!
//! Because the decision is made at run time from the current directory, shims
//! behave correctly even in shells with no directory hook (notably `cmd.exe`),
//! and uninstalling rbuild leaves nothing that could alter a normal command.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rbuild_proto::config::GlobalConfig;
use rbuild_proto::proto::Target;

/// The command name a shim was invoked as, or None when running as plain
/// `rbuild`. On Unix the shim is a symlink, so the name is `argv[0]`. On Windows
/// the `.cmd` forwarder calls `rbuild.exe` directly, so it passes the name via
/// `RBUILD_SHIM_AS` instead — checked first so it works on every platform.
pub fn invoked_as() -> Option<String> {
    if let Some(cmd) = std::env::var_os("RBUILD_SHIM_AS") {
        let cmd = cmd.to_string_lossy().to_string();
        if !cmd.is_empty() {
            return Some(cmd);
        }
    }
    let arg0 = std::env::args_os().next()?;
    let name = Path::new(&arg0).file_name()?.to_string_lossy().to_string();
    let stem = name.strip_suffix(".exe").unwrap_or(&name);
    if stem == "rbuild" {
        None
    } else {
        Some(stem.to_string())
    }
}

/// Entry point for shim-mode execution. Never returns on the pass-through path
/// (it execs the real binary); on the offload path it returns the build's exit
/// code for the caller to propagate.
pub async fn run_as_shim(command: String) -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // Offload only when inside a registered code root and the command is one we
    // intercept; otherwise the real tool runs locally, untouched.
    let cfg = GlobalConfig::load_or_default();
    let offload = cfg
        .as_ref()
        .map(|c| c.locate(&cwd).is_some() && c.build.commands.iter().any(|x| x == &command))
        .unwrap_or(false);

    if offload {
        let mut full = vec![command.clone()];
        full.extend(argv.iter().cloned());
        // Target follows the client OS automatically.
        match crate::cmd_build(full, Target::host()).await {
            Ok(code) => return Ok(code),
            Err(e) if is_remote_unreachable(&e) => {
                eprintln!(
                    "rbuild: remote unreachable ({}); building locally instead.",
                    short_cause(&e)
                );
                return exec_real(&command, &argv);
            }
            Err(e) => return Err(e),
        }
    }

    exec_real(&command, &argv)
}

/// Distinguishes "the remote is unavailable" (fall back to local) from a real
/// build/usage error (which must surface, not silently rebuild locally).
fn is_remote_unreachable(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_lowercase();
    msg.contains("did not respond")
        || msg.contains("could not resolve hostname")
        || msg.contains("connection")
        || msg.contains("timed out")
        || msg.contains("no route to host")
        || msg.contains("connection refused")
        || msg.contains("launching ssh")
}

fn short_cause(err: &anyhow::Error) -> String {
    err.to_string().lines().next().unwrap_or("unreachable").to_string()
}

/// Locates the real command on PATH, skipping the rbuild shim directory, and
/// execs it. Returns an error only if the command can't be found or launched.
fn exec_real(command: &str, argv: &[String]) -> Result<i32> {
    let real = find_real_on_path(command)
        .with_context(|| format!("`{command}` not found on PATH (outside rbuild shims)"))?;

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&real).args(argv).exec();
        Err(err).with_context(|| format!("exec {}", real.display()))
    }

    #[cfg(not(unix))]
    {
        // Clear the shim marker so the real tool and its children never see it.
        let status = std::process::Command::new(&real)
            .args(argv)
            .env_remove("RBUILD_SHIM_AS")
            .status()
            .with_context(|| format!("running {}", real.display()))?;
        Ok(status.code().unwrap_or(-1))
    }
}

/// Searches PATH for `command`, ignoring the rbuild shim dir so a shim never
/// resolves to itself.
fn find_real_on_path(command: &str) -> Option<PathBuf> {
    let shim_dir = GlobalConfig::shim_dir().ok();
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if Some(&dir) == shim_dir.as_ref() {
            continue;
        }
        for candidate in candidates(&dir, command) {
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Candidate filenames for a command in a directory. On Windows a bare name may
/// resolve to `name.exe`, `name.cmd`, or `name.bat`.
fn candidates(dir: &Path, command: &str) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        let mut out = Vec::new();
        for ext in exts.split(';') {
            let mut name = OsString::from(command);
            name.push(ext.to_lowercase());
            out.push(dir.join(&name));
        }
        out.push(dir.join(command));
        out
    }
    #[cfg(not(windows))]
    {
        let _ = OsString::new();
        vec![dir.join(command)]
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// (Re)builds the global shim directory so it contains exactly one shim per
/// intercepted command, each pointing at the current `rbuild` binary. Clearing
/// first means commands removed from the config leave no stale shim behind.
pub fn install_shims(commands: &[String]) -> Result<()> {
    let dir = GlobalConfig::shim_dir()?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    let exe = std::env::current_exe().context("locating rbuild binary")?;
    for cmd in commands {
        link_shim(&exe, &dir, cmd)?;
    }
    Ok(())
}

/// Removes the global shim directory entirely.
pub fn remove_shims() -> Result<()> {
    let dir = GlobalConfig::shim_dir()?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(())
}

#[cfg(unix)]
fn link_shim(exe: &Path, dir: &Path, command: &str) -> Result<()> {
    let link = dir.join(command);
    std::os::unix::fs::symlink(exe, &link)
        .with_context(|| format!("linking shim {}", link.display()))
}

#[cfg(windows)]
fn link_shim(exe: &Path, dir: &Path, command: &str) -> Result<()> {
    // Symlinks need privilege on Windows, and a `.cmd` forwarder invokes
    // rbuild.exe directly — so argv[0] would be "rbuild", not the command name.
    // The forwarder therefore tells rbuild which command it stands in for via
    // RBUILD_SHIM_AS, which invoked_as() reads. `%*` forwards the args; the
    // env var is scoped to this invocation by cmd's process environment.
    let script = dir.join(format!("{command}.cmd"));
    let body = format!(
        "@echo off\r\nset \"RBUILD_SHIM_AS={command}\"\r\n\"{}\" %*\r\n",
        exe.display()
    );
    std::fs::write(&script, body)
        .with_context(|| format!("writing shim {}", script.display()))
}
