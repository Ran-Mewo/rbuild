//! Shell integration: a direnv-style hook that puts a linked project's shim
//! directory on PATH only while the shell is inside that project, and removes
//! it on leaving. The hook calls back into `rbuild hook <shell>` on each
//! prompt to print the shell code that adjusts PATH.
//!
//! All installs are idempotent and uninstall-clean: edits live between stable
//! markers and are rewritten (never appended) so re-running never duplicates,
//! and removal matches the markers regardless of how install ran.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const MARKER_BEGIN: &str = "# >>> rbuild >>>";
const MARKER_END: &str = "# <<< rbuild <<<";

/// Shells rbuild can integrate with.
#[allow(clippy::enum_variant_names)] // "PowerShell" is the product's real name
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    PowerShell,
    Cmd,
}

impl Shell {
    pub fn parse(s: &str) -> Option<Shell> {
        match s.to_lowercase().as_str() {
            "bash" => Some(Shell::Bash),
            "zsh" => Some(Shell::Zsh),
            "fish" => Some(Shell::Fish),
            "powershell" | "pwsh" => Some(Shell::PowerShell),
            "cmd" => Some(Shell::Cmd),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
            Shell::PowerShell => "powershell",
            Shell::Cmd => "cmd",
        }
    }
}

/// The rc/profile file a shell's hook line is written to.
fn rc_path(shell: Shell) -> Result<PathBuf> {
    let home = dirs_home().context("could not determine home directory")?;
    Ok(match shell {
        Shell::Bash => home.join(".bashrc"),
        Shell::Zsh => home.join(".zshrc"),
        Shell::Fish => home.join(".config/fish/config.fish"),
        // The well-known cross-edition PowerShell profile location.
        Shell::PowerShell => home
            .join("Documents")
            .join("PowerShell")
            .join("Microsoft.PowerShell_profile.ps1"),
        // cmd has no rc file; handled via the AutoRun registry value instead.
        Shell::Cmd => anyhow::bail!("cmd uses AutoRun, not an rc file"),
    })
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// The one line a shell sources on each prompt. It asks `rbuild` for the PATH
/// adjustment appropriate to the current directory and applies it.
fn hook_block(shell: Shell, rbuild_bin: &Path) -> String {
    let bin = rbuild_bin.display();
    match shell {
        Shell::Bash | Shell::Zsh => format!(
            "{MARKER_BEGIN}\n\
             _rbuild_hook() {{ eval \"$(\"{bin}\" hook {})\"; }}\n\
             PROMPT_COMMAND=\"_rbuild_hook;${{PROMPT_COMMAND:-}}\"\n\
             {MARKER_END}\n",
            shell.name()
        ),
        Shell::Fish => format!(
            "{MARKER_BEGIN}\n\
             function _rbuild_hook --on-event fish_prompt\n\
             \x20   {bin} hook fish | source\n\
             end\n\
             {MARKER_END}\n"
        ),
        Shell::PowerShell => format!(
            "{MARKER_BEGIN}\n\
             if (-not (Test-Path Function:\\_rbuild_orig_prompt)) {{ Copy-Item Function:\\prompt Function:\\_rbuild_orig_prompt -ErrorAction SilentlyContinue }}\n\
             function global:prompt {{ & \"{bin}\" hook powershell | Out-String | Invoke-Expression; if (Test-Path Function:\\_rbuild_orig_prompt) {{ _rbuild_orig_prompt }} else {{ \"PS $($executionContext.SessionState.Path.CurrentLocation)$('>' * ($nestedPromptLevel + 1)) \" }} }}\n\
             {MARKER_END}\n"
        ),
        Shell::Cmd => String::new(),
    }
}

/// Installs (or refreshes) the hook for a shell. Idempotent: strips any prior
/// rbuild block first, then writes exactly one.
pub fn install(shell: Shell, rbuild_bin: &Path) -> Result<()> {
    if shell == Shell::Cmd {
        return install_cmd(rbuild_bin);
    }
    let path = rc_path(shell)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let cleaned = strip_block(&existing);
    let block = hook_block(shell, rbuild_bin);
    let mut next = cleaned;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(&block);
    std::fs::write(&path, next).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Removes the hook for a shell. Safe to run when nothing is installed.
pub fn uninstall(shell: Shell) -> Result<()> {
    if shell == Shell::Cmd {
        return uninstall_cmd();
    }
    let path = rc_path(shell)?;
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let cleaned = strip_block(&existing);
        if cleaned != existing {
            std::fs::write(&path, cleaned)?;
        }
    }
    Ok(())
}

/// Removes a complete `MARKER_BEGIN..MARKER_END` block (and the blank line it
/// leaves) from `text`, returning the remainder. Tolerates multiple blocks in
/// case an older buggy install left more than one.
fn strip_block(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut skipping = false;
    for line in text.lines() {
        if line.trim() == MARKER_BEGIN {
            skipping = true;
            continue;
        }
        if line.trim() == MARKER_END {
            skipping = false;
            continue;
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// The PATH-adjustment code printed for the current directory. Stored PATH is
/// restored on leaving a code root by remembering what we added in an env var.
pub fn emit_hook(shell: Shell) -> Result<String> {
    let cwd = std::env::current_dir()?;
    // Inside a registered code root → expose the shim dir; otherwise remove it.
    let inside = rbuild_proto::config::GlobalConfig::load_or_default()
        .map(|c| c.locate(&cwd).is_some())
        .unwrap_or(false);
    let target = if inside {
        rbuild_proto::config::GlobalConfig::shim_dir().ok()
    } else {
        None
    };

    Ok(match shell {
        Shell::Bash | Shell::Zsh => bash_hook(target.as_deref()),
        Shell::Fish => fish_hook(target.as_deref()),
        Shell::PowerShell => powershell_hook(target.as_deref()),
        Shell::Cmd => String::new(),
    })
}

/// We track the shim dir we injected in `RBUILD_SHIM_DIR` so the next prompt can
/// remove exactly that entry — never touching the user's own PATH entries.
fn bash_hook(target: Option<&Path>) -> String {
    let mut s = String::new();
    // Remove any previously-injected shim dir from PATH.
    s.push_str(
        "if [ -n \"$RBUILD_SHIM_DIR\" ]; then \
         PATH=$(printf '%s' \"$PATH\" | tr ':' '\\n' | grep -vxF \"$RBUILD_SHIM_DIR\" | paste -sd:); \
         unset RBUILD_SHIM_DIR; fi\n",
    );
    if let Some(dir) = target {
        s.push_str(&format!(
            "export RBUILD_SHIM_DIR='{0}'; export PATH='{0}':\"$PATH\"\n",
            dir.display()
        ));
    }
    s
}

fn fish_hook(target: Option<&Path>) -> String {
    let mut s = String::new();
    s.push_str(
        "if set -q RBUILD_SHIM_DIR; \
         set PATH (string match -v -- $RBUILD_SHIM_DIR $PATH); \
         set -e RBUILD_SHIM_DIR; end\n",
    );
    if let Some(dir) = target {
        s.push_str(&format!(
            "set -gx RBUILD_SHIM_DIR '{0}'; set -gx PATH '{0}' $PATH\n",
            dir.display()
        ));
    }
    s
}

fn powershell_hook(target: Option<&Path>) -> String {
    let mut s = String::new();
    s.push_str(
        "if ($env:RBUILD_SHIM_DIR) { \
         $env:PATH = ($env:PATH -split ';' | Where-Object { $_ -ne $env:RBUILD_SHIM_DIR }) -join ';'; \
         Remove-Item Env:\\RBUILD_SHIM_DIR } ",
    );
    if let Some(dir) = target {
        s.push_str(&format!(
            "$env:RBUILD_SHIM_DIR = '{0}'; $env:PATH = '{0}' + ';' + $env:PATH",
            dir.display()
        ));
    }
    s
}

// --- cmd.exe via the AutoRun registry value -------------------------------
//
// cmd has no per-directory hook, so we can't add/remove the shim dir per
// directory the way the other shells do. But the shims themselves do a runtime
// cwd check (offload only inside a code root, else exec the real tool), so it's
// safe to keep the shim dir on PATH for all cmd sessions. We prepend it via the
// command-processor AutoRun value, which cmd runs at every startup. Only
// rbuild's own `&`-separated segment is touched, preserving any the user has.

#[cfg(windows)]
fn cmd_segment() -> Result<String> {
    let shim_dir = rbuild_proto::config::GlobalConfig::shim_dir()?;
    // Marker tags our segment so install/uninstall can find exactly it.
    Ok(format!(
        "set \"PATH={};%PATH%\" {}",
        shim_dir.display(),
        MARKER_BEGIN
    ))
}

#[cfg(windows)]
fn install_cmd(_rbuild_bin: &Path) -> Result<()> {
    use std::process::Command;
    let segment = cmd_segment()?;
    let existing = read_autorun().unwrap_or_default();
    let cleaned = strip_autorun_segment(&existing);
    let combined = if cleaned.is_empty() {
        segment
    } else {
        format!("{cleaned}&{segment}")
    };
    let status = Command::new("reg")
        .args([
            "add",
            r"HKCU\Software\Microsoft\Command Processor",
            "/v",
            "AutoRun",
            "/t",
            "REG_SZ",
            "/d",
            &combined,
            "/f",
        ])
        .status()
        .context("running reg add for cmd AutoRun")?;
    if !status.success() {
        anyhow::bail!("reg add failed");
    }
    Ok(())
}

#[cfg(windows)]
fn uninstall_cmd() -> Result<()> {
    use std::process::Command;
    let existing = read_autorun().unwrap_or_default();
    let cleaned = strip_autorun_segment(&existing);
    if cleaned.is_empty() {
        Command::new("reg")
            .args([
                "delete",
                r"HKCU\Software\Microsoft\Command Processor",
                "/v",
                "AutoRun",
                "/f",
            ])
            .status()
            .ok();
    } else if cleaned != existing {
        Command::new("reg")
            .args([
                "add",
                r"HKCU\Software\Microsoft\Command Processor",
                "/v",
                "AutoRun",
                "/t",
                "REG_SZ",
                "/d",
                &cleaned,
                "/f",
            ])
            .status()
            .ok();
    }
    Ok(())
}

#[cfg(windows)]
fn read_autorun() -> Option<String> {
    use std::process::Command;
    let out = Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Command Processor",
            "/v",
            "AutoRun",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // The value is the last whitespace-separated field on the AutoRun line.
    text.lines()
        .find(|l| l.contains("AutoRun"))
        .and_then(|l| l.split("REG_SZ").nth(1))
        .map(|v| v.trim().to_string())
}

/// Drops rbuild's `&`-separated AutoRun segment (the one tagged with our
/// marker), preserving any other segments the user has. Pure string logic, so
/// it's compiled and tested on every platform even though only cmd uses it.
#[cfg_attr(not(windows), allow(dead_code))]
fn strip_autorun_segment(value: &str) -> String {
    value
        .split('&')
        .filter(|seg| !seg.contains(MARKER_BEGIN))
        .collect::<Vec<_>>()
        .join("&")
}

// Non-Windows stubs so the CLI surface is uniform; cmd only exists on Windows.
#[cfg(not(windows))]
fn install_cmd(_rbuild_bin: &Path) -> Result<()> {
    anyhow::bail!("cmd integration is only available on Windows")
}

#[cfg(not(windows))]
fn uninstall_cmd() -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_is_idempotent_and_total() {
        let base = "export FOO=1\n";
        let block = hook_block(Shell::Bash, Path::new("/usr/bin/rbuild"));
        let installed = format!("{base}{block}");
        // Stripping removes exactly the block, leaving the original content.
        assert_eq!(strip_block(&installed), base);
        // Double-install then strip still yields the base — no accumulation.
        let twice = format!("{base}{block}{block}");
        assert_eq!(strip_block(&twice), base);
    }

    #[test]
    fn autorun_segment_strip_preserves_others() {
        // A user with their own AutoRun plus rbuild's segment.
        let ours = format!("set \"PATH=C:\\rb;%PATH%\" {MARKER_BEGIN}");
        let theirs = "doskey ls=dir $*";
        let combined = format!("{theirs}&{ours}");
        // Stripping leaves the user's segment untouched.
        assert_eq!(strip_autorun_segment(&combined), theirs);
        // Stripping when only ours is present yields empty (triggers reg delete).
        assert_eq!(strip_autorun_segment(&ours), "");
        // Stripping when ours is absent is a no-op.
        assert_eq!(strip_autorun_segment(theirs), theirs);
    }
}
