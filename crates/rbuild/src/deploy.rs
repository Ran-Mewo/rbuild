//! Bootstrapping rbuild's remote side — entirely in Docker, nothing on the
//! host filesystem.
//!
//! The daemon is a static musl binary stored in a labelled Docker volume
//! (`rbuild-bin`). The daemon, and every build it launches, run in containers
//! that mount labelled named volumes for the workspace and cache. So the
//! remote's only footprint is Docker's own storage: `rbuild uninstall` removes
//! the labelled volumes and images and the host is exactly as it was.

use std::process::Stdio;

use anyhow::{Context, Result};
use rbuild_proto::config::RemoteConfig;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Label put on every volume and image rbuild creates, so uninstall can find
/// and remove them all without guessing names.
pub const LABEL: &str = "rbuild=true";

/// Minimal image used to host the daemon and run cleanup. `docker:cli` carries
/// the docker client the daemon needs to launch sibling build containers.
pub const DAEMON_IMAGE: &str = "docker:cli";

/// Tiny image for plain file operations (streaming the daemon binary into its
/// volume). Kept separate from the daemon image because its entrypoint is a
/// shell, not the docker client.
pub const HELPER_IMAGE: &str = "alpine";

/// Names of the labelled volumes. The binary volume is shared; workspace and
/// cache volumes are per workspace id.
pub const BIN_VOLUME: &str = "rbuild-bin";

pub fn ws_volume(workspace_id: &str) -> String {
    format!("rbuild-ws-{workspace_id}")
}

pub fn cache_volume(workspace_id: &str) -> String {
    format!("rbuild-cache-{workspace_id}")
}

fn ssh_base(remote: &RemoteConfig) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.arg("-o").arg("BatchMode=yes");
    if let Some(id) = &remote.identity_file {
        cmd.arg("-i").arg(id);
    }
    cmd.arg(&remote.host);
    cmd
}

/// The Docker invocation to use on the remote — `docker` or `sudo docker`.
/// Uses the cached value from config when present, otherwise `docker`.
pub fn docker_cmd(remote: &RemoteConfig) -> &str {
    if remote.docker.is_empty() {
        "docker"
    } else {
        &remote.docker
    }
}

/// Detects how Docker must be invoked on the remote: plain `docker` if that
/// works, else `sudo docker` (for hosts where the user isn't in the docker
/// group but has passwordless sudo). Returns the working prefix, or an error if
/// neither works. The result is cached in config by the caller.
pub async fn detect_docker(remote: &RemoteConfig) -> Result<String> {
    for candidate in ["docker", "sudo docker"] {
        let probe = format!("{candidate} version --format '{{{{.Server.Version}}}}'");
        if ssh_base(remote).arg(&probe).output().await
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            return Ok(candidate.to_string());
        }
    }
    anyhow::bail!(
        "Docker not usable on {} as `docker` or `sudo docker`. \
         Ensure Docker is installed and your SSH user can run it (docker group, \
         or passwordless sudo).",
        remote.host
    )
}

/// Runs a remote command, returning trimmed stdout. `args` are passed to ssh,
/// which concatenates them and runs the result through the remote login shell.
async fn ssh_capture(remote: &RemoteConfig, args: &[&str]) -> Result<String> {
    let out = ssh_base(remote).args(args).output().await.context("running ssh")?;
    if !out.status.success() {
        anyhow::bail!("ssh {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Runs a shell script on the remote (passed as one argument so SSH hands it to
/// the remote login shell intact), streaming output to the user's terminal.
pub async fn run_remote_script(remote: &RemoteConfig, script: &str) -> Result<()> {
    let status = ssh_base(remote).arg(script).status().await.context("running ssh")?;
    if !status.success() {
        anyhow::bail!("remote command failed");
    }
    Ok(())
}

/// Whether the daemon binary volume already exists on the remote. Used to
/// decide if a deploy is needed before launching the daemon container.
pub async fn bin_volume_present(remote: &RemoteConfig) -> Result<bool> {
    let docker = docker_cmd(remote);
    let script = format!("{docker} volume ls -q -f name=^{BIN_VOLUME}$");
    let out = ssh_capture(remote, &[&script]).await?;
    Ok(out.lines().any(|l| l.trim() == BIN_VOLUME))
}

/// Ensures the remote has Docker and the static daemon binary loaded into the
/// `rbuild-bin` volume. Idempotent: re-running overwrites the binary in place,
/// so a re-deploy after any change never duplicates state.
pub async fn ensure_daemon(remote: &RemoteConfig) -> Result<()> {
    let docker = docker_cmd(remote);
    let bytes = daemon_binary()?;

    // Create the labelled bin volume, then stream the daemon into it through a
    // throwaway container. We override the helper image's entrypoint to `sh` so
    // the redirect runs in a real shell (the default `docker` entrypoint would
    // treat `cp`/`cat` as a docker subcommand). The inner script uses only
    // double quotes, so it survives SSH concatenating the command into one
    // string for the remote login shell.
    let script = format!(
        "{docker} volume create --label {LABEL} {BIN_VOLUME} >/dev/null && \
         {docker} run --rm -i -v {BIN_VOLUME}:/out --entrypoint sh {HELPER_IMAGE} \
             -c \"cat > /out/rbuildd && chmod 755 /out/rbuildd\""
    );

    let mut child = ssh_base(remote)
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning ssh for daemon upload")?;
    {
        let mut stdin = child.stdin.take().context("ssh stdin missing")?;
        stdin.write_all(&bytes).await?;
        stdin.flush().await?;
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        anyhow::bail!("daemon upload failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

/// Locates the static musl daemon binary built by `scripts/build-daemon.sh`.
///
/// The daemon runs in a minimal (musl) container, so it must be the static
/// build, never the host's dynamically-linked `target/<profile>/rbuildd`. We
/// therefore look only in the dedicated `target/daemon/` output location (and
/// a `daemon/` dir beside a packaged `rbuild`), not next to the dev binary.
fn daemon_binary() -> Result<Vec<u8>> {
    let exe = std::env::current_exe().context("locating rbuild binary")?;
    let dir = exe.parent().context("rbuild has no parent dir")?;
    let candidates = [
        // Packaged layout: rbuild and daemon/rbuildd shipped side by side.
        dir.join("daemon").join("rbuildd"),
        // Cargo dev layout: target/<profile>/rbuild → target/daemon/rbuildd.
        dir.join("..").join("daemon").join("rbuildd"),
    ];
    for c in &candidates {
        if c.is_file() {
            return std::fs::read(c).with_context(|| format!("reading {}", c.display()));
        }
    }
    anyhow::bail!(
        "static daemon binary not found (looked in target/daemon/). \
         Build it with `scripts/build-daemon.sh`."
    )
}

/// Removes every labelled volume and rbuild image from the remote — the entire
/// remote footprint. Safe to run repeatedly.
pub async fn wipe_remote(remote: &RemoteConfig) -> Result<()> {
    // One shell command, single round trip: remove all labelled volumes, then
    // rbuild's images. Each step tolerates "nothing to remove".
    let docker = docker_cmd(remote);
    let script = format!(
        "vols=$({docker} volume ls -q -f label={LABEL}); \
         [ -n \"$vols\" ] && {docker} volume rm -f $vols >/dev/null 2>&1; \
         {docker} rmi rbuild/linux:latest rbuild/wine:latest >/dev/null 2>&1; \
         true"
    );
    run_remote_script(remote, &script).await
}

/// Stops every running daemon container. After re-pushing a newer daemon binary
/// we kill the old daemons so their next reconnect launches a fresh container
/// running the new binary — this is what actually rolls the running daemon
/// forward, not just the binary in the volume. Build containers are untouched
/// (only `rbuild.role=daemon` is targeted).
pub async fn kill_daemons(remote: &RemoteConfig) -> Result<()> {
    let docker = docker_cmd(remote);
    let script = format!(
        "ids=$({docker} ps -q -f label=rbuild.role=daemon); \
         [ -n \"$ids\" ] && {docker} kill $ids >/dev/null 2>&1; \
         true"
    );
    run_remote_script(remote, &script).await
}
