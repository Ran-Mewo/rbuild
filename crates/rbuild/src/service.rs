//! OS login service that runs the live-sync agent.
//!
//! The agent must start at login and keep running so edits propagate without
//! any terminal open. We register it with the platform's user-level service
//! manager — systemd (Linux), launchd (macOS), Task Scheduler (Windows) — all
//! at the per-user level so no root/admin is needed and uninstall is clean.

use anyhow::{Context, Result};

/// A stable identifier for the service across platforms.
const SERVICE_NAME: &str = "rbuild-agent";

/// Installs (idempotently) and starts the login service that runs `rbuild
/// agent`. Best-effort: returns an error with guidance if the platform's
/// service manager isn't available, but callers treat that as non-fatal.
pub fn install() -> Result<()> {
    // Escape hatch for tests/CI and for users who manage the agent themselves.
    if std::env::var_os("RBUILD_NO_SERVICE").is_some() {
        return Ok(());
    }
    let exe = std::env::current_exe().context("locating rbuild binary")?;
    platform::install(&exe)
}

/// Stops and removes the login service. Safe to call when not installed.
pub fn uninstall() -> Result<()> {
    if std::env::var_os("RBUILD_NO_SERVICE").is_some() {
        return Ok(());
    }
    platform::uninstall()
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    fn unit_path() -> Result<PathBuf> {
        let home = std::env::var("HOME").context("HOME not set")?;
        Ok(PathBuf::from(home).join(".config/systemd/user").join(format!("{SERVICE_NAME}.service")))
    }

    pub fn install(exe: &std::path::Path) -> Result<()> {
        let path = unit_path()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        // Restart=always keeps the agent up across crashes; the agent itself
        // tolerates an unconfigured/offline state, so starting early is fine.
        let unit = format!(
            "[Unit]\n\
             Description=rbuild live-sync agent\n\
             After=network-online.target\n\n\
             [Service]\n\
             ExecStart={exe} agent\n\
             Restart=always\n\
             RestartSec=3\n\n\
             [Install]\n\
             WantedBy=default.target\n",
            exe = exe.display()
        );
        std::fs::write(&path, unit).with_context(|| format!("writing {}", path.display()))?;

        // Reload, enable, and start. If the user systemd bus isn't present
        // (e.g. headless without lingering), surface a clear hint.
        run("systemctl", &["--user", "daemon-reload"])?;
        run("systemctl", &["--user", "enable", "--now", SERVICE_NAME])
            .context("starting the agent service (try `loginctl enable-linger $USER` on headless hosts)")?;
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        // Best-effort stop/disable, then remove the unit file.
        let _ = run("systemctl", &["--user", "disable", "--now", SERVICE_NAME]);
        if let Ok(path) = unit_path() {
            let _ = std::fs::remove_file(path);
        }
        let _ = run("systemctl", &["--user", "daemon-reload"]);
        Ok(())
    }

    fn run(cmd: &str, args: &[&str]) -> Result<()> {
        let status = Command::new(cmd).args(args).status()
            .with_context(|| format!("running {cmd} {args:?}"))?;
        if !status.success() {
            anyhow::bail!("{cmd} {args:?} failed");
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::path::PathBuf;

    fn plist_path() -> Result<PathBuf> {
        let home = std::env::var("HOME").context("HOME not set")?;
        Ok(PathBuf::from(home)
            .join("Library/LaunchAgents")
            .join(format!("com.rbuild.{SERVICE_NAME}.plist")))
    }

    fn label() -> String {
        format!("com.rbuild.{SERVICE_NAME}")
    }

    pub fn install(exe: &std::path::Path) -> Result<()> {
        let path = plist_path()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let plist = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\"><dict>\n\
             <key>Label</key><string>{label}</string>\n\
             <key>ProgramArguments</key><array><string>{exe}</string><string>agent</string></array>\n\
             <key>RunAtLoad</key><true/>\n\
             <key>KeepAlive</key><true/>\n\
             </dict></plist>\n",
            label = label(),
            exe = exe.display()
        );
        std::fs::write(&path, plist)?;
        // bootstrap into the user's GUI domain; ignore "already loaded".
        let uid = format!("gui/{}", unsafe { libc::getuid() });
        let _ = std::process::Command::new("launchctl")
            .args(["bootstrap", &uid, &path.to_string_lossy()])
            .status();
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        if let Ok(path) = plist_path() {
            let uid = format!("gui/{}", unsafe { libc::getuid() });
            let _ = std::process::Command::new("launchctl")
                .args(["bootout", &uid, &path.to_string_lossy()])
                .status();
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use std::process::Command;

    pub fn install(exe: &std::path::Path) -> Result<()> {
        // A per-user logon scheduled task; /f overwrites so install is idempotent.
        let tr = format!("\"{}\" agent", exe.display());
        let status = Command::new("schtasks")
            .args([
                "/Create", "/SC", "ONLOGON", "/TN", SERVICE_NAME,
                "/TR", &tr, "/F",
            ])
            .status()
            .context("running schtasks")?;
        if !status.success() {
            anyhow::bail!("schtasks create failed");
        }
        // Start it now too, so the user doesn't have to log out/in first.
        let _ = Command::new("schtasks").args(["/Run", "/TN", SERVICE_NAME]).status();
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        let _ = Command::new("schtasks").args(["/End", "/TN", SERVICE_NAME]).status();
        let _ = Command::new("schtasks").args(["/Delete", "/TN", SERVICE_NAME, "/F"]).status();
        Ok(())
    }
}
