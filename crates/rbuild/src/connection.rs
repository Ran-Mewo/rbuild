//! Establishing the daemon connection over SSH.
//!
//! We never open a port. The client SSHes in and launches the daemon as a
//! Docker container (`docker run … rbuildd serve`), speaking the frame protocol
//! over that process's stdin/stdout. The daemon container mounts the Docker
//! socket — so it can launch sibling build containers — plus the labelled
//! workspace and cache volumes. Nothing is written to the remote host FS.

use std::process::Stdio;

use anyhow::{Context, Result};
use rbuild_proto::config::RemoteConfig;
use rbuild_proto::proto::{Message, PROTOCOL_VERSION};
use rbuild_proto::transport::{read_frame, write_frame, Frame};
use rbuild_proto::VERSION;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::deploy;

/// A live, handshaked connection to a remote `rbuildd`.
pub struct Connection {
    child: Child,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub daemon_version: String,
}

impl Connection {
    /// Connects for a given workspace, ensuring Docker access is detected and
    /// the daemon binary is present in its Docker volume first. This is the path
    /// normal commands use.
    pub async fn connect_or_deploy(remote: &RemoteConfig, workspace_id: &str) -> Result<Self> {
        // Resolve how Docker is invoked on the remote (`docker` vs `sudo
        // docker`) once, caching it in config so later runs skip the probe.
        let remote = ensure_docker_detected(remote).await?;

        // Deploy first when the bin volume is absent. We must not just let a
        // failed connect trigger deploy: launching the daemon container mounts
        // the bin volume, which would auto-create it *unlabelled*, and a later
        // `docker volume create --label` is a no-op on an existing volume.
        if !deploy::bin_volume_present(&remote).await.unwrap_or(false) {
            tracing::info!("daemon not present on remote — deploying");
            deploy::ensure_daemon(&remote)
                .await
                .context("auto-deploying rbuildd")?;
        }
        Self::connect(&remote, workspace_id).await
    }

    /// Launches the daemon container over SSH and completes the handshake.
    pub async fn connect(remote: &RemoteConfig, workspace_id: &str) -> Result<Self> {
        let mut cmd = Command::new("ssh");
        // Batch mode so a missing key fails fast instead of hanging on a
        // password prompt that has nowhere to read from.
        cmd.arg("-o").arg("BatchMode=yes");
        if let Some(id) = &remote.identity_file {
            cmd.arg("-i").arg(id);
        }
        cmd.arg(&remote.host);
        // The remote command: ensure the per-workspace volumes exist, then run
        // the daemon container with the socket and volumes mounted. Passed as a
        // single argument so SSH hands it to the remote login shell intact.
        cmd.arg(launch_script(deploy::docker_cmd(remote), workspace_id));
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("launching ssh to {}", remote.host))?;
        let mut stdin = child.stdin.take().context("ssh stdin missing")?;
        let mut stdout = child.stdout.take().context("ssh stdout missing")?;
        let mut stderr = child.stderr.take().context("ssh stderr missing")?;

        // Stream 0 is reserved for the connection-level handshake.
        let hello = Message::Hello {
            protocol: PROTOCOL_VERSION,
            client_version: VERSION.to_string(),
        };
        write_frame(&mut stdin, &Frame::control(0, &hello)?).await?;

        let frame = match read_frame(&mut stdout).await? {
            Some(f) => f,
            None => {
                // Daemon never spoke. Surface whatever the remote printed so
                // the caller can detect a missing-binary situation and deploy.
                let mut buf = String::new();
                use tokio::io::AsyncReadExt;
                let _ = stderr.read_to_string(&mut buf).await;
                let _ = child.wait().await;
                anyhow::bail!("daemon did not respond. remote said: {}", buf.trim());
            }
        };
        // Once the handshake is reading, drain stderr in the background so the
        // daemon's logs reach the user's terminal without blocking the pipe.
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = [0u8; 4096];
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        use std::io::Write;
                        let _ = std::io::stderr().write_all(&buf[..n]);
                    }
                }
            }
        });
        let daemon_version = match frame.as_message()? {
            Message::Welcome {
                protocol,
                daemon_version,
                ok,
                message,
            } => {
                if !ok || protocol != PROTOCOL_VERSION {
                    anyhow::bail!(
                        "daemon rejected handshake (protocol {protocol}, ours {PROTOCOL_VERSION}): {message}"
                    );
                }
                daemon_version
            }
            other => anyhow::bail!("expected Welcome, got {other:?}"),
        };

        Ok(Connection {
            child,
            stdin,
            stdout,
            daemon_version,
        })
    }

    /// Closes stdin so the daemon container sees EOF, exits, and is removed.
    pub async fn shutdown(mut self) -> Result<()> {
        drop(self.stdin);
        let _ = self.child.wait().await;
        Ok(())
    }
}

/// The remote shell command that creates the workspace's volumes (idempotent)
/// and runs the daemon container. `docker` is the client-side invocation
/// (`docker` or `sudo docker`); inside the launched container the daemon uses
/// plain `docker` against the mounted socket, so no sudo is needed there.
fn launch_script(docker: &str, workspace_id: &str) -> String {
    let bin = deploy::BIN_VOLUME;
    let ws = deploy::ws_volume(workspace_id);
    let cache = deploy::cache_volume(workspace_id);
    let img = deploy::DAEMON_IMAGE;
    let label = deploy::LABEL;
    format!(
        "{docker} volume create --label {label} {ws} >/dev/null; \
         {docker} volume create --label {label} {cache} >/dev/null; \
         exec {docker} run --rm -i \
             -v /var/run/docker.sock:/var/run/docker.sock \
             -v {bin}:/rbuild-bin \
             -v {ws}:/work \
             -v {cache}:/cache \
             -e RBUILD_WORK=/work \
             -e RBUILD_WS_VOLUME={ws} \
             -e RBUILD_CACHE_VOLUME={cache} \
             {img} /rbuild-bin/rbuildd serve"
    )
}

/// Ensures `remote.docker` is populated, probing the remote and caching the
/// result to the global config so subsequent runs skip the probe. Returns a
/// copy of the remote config with `.docker` set.
async fn ensure_docker_detected(remote: &RemoteConfig) -> Result<RemoteConfig> {
    if !remote.docker.is_empty() {
        return Ok(remote.clone());
    }
    let docker = deploy::detect_docker(remote).await?;
    let mut remote = remote.clone();
    remote.docker = docker;
    // Persist so future invocations don't re-probe; ignore save errors (the
    // detection still holds for this run).
    if let Ok(mut cfg) = rbuild_proto::config::GlobalConfig::load() {
        cfg.remote.docker = remote.docker.clone();
        let _ = cfg.save();
    }
    Ok(remote)
}

