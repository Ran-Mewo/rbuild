//! Building the remote build images on demand.
//!
//! The Dockerfiles and Wine wrappers are embedded in the client, so it can
//! build `rbuild/linux` and `rbuild/wine` on the remote itself — over the same
//! SSH connection, streaming an in-memory tar build context into `docker build
//! -`. Nothing is written to the remote host filesystem; the resulting image
//! lives in Docker's own storage and is labelled for clean uninstall.

use std::process::Stdio;

use anyhow::{Context, Result};
use rbuild_proto::config::RemoteConfig;
use rbuild_proto::proto::Target;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::deploy::{docker_cmd, LABEL};

// Build contexts embedded at compile time, so a released binary is self-
// contained — no repo checkout needed on the user's machine or the remote.
const LINUX_DOCKERFILE: &str = include_str!("../../../docker/linux.Dockerfile");
const WINE_DOCKERFILE: &str = include_str!("../../../docker/wine.Dockerfile");
const WINE_COMMON: &str = include_str!("../../../docker/wine-wrappers/_common.sh");
const WINE_CARGO: &str = include_str!("../../../docker/wine-wrappers/cargo");
const WINE_RUSTC: &str = include_str!("../../../docker/wine-wrappers/rustc");

fn image_name(target: Target) -> &'static str {
    match target {
        Target::Linux => "rbuild/linux:latest",
        Target::Windows => "rbuild/wine:latest",
    }
}

/// Ensures the build image for `target` exists on the remote, building it if
/// not. Idempotent and safe to call before every build — the existence check is
/// a cheap `docker image inspect`.
pub async fn ensure_image(remote: &RemoteConfig, target: Target) -> Result<()> {
    let docker = docker_cmd(remote);
    let image = image_name(target);

    if image_present(remote, docker, image).await? {
        return Ok(());
    }

    let kind = match target {
        Target::Linux => "Linux",
        Target::Windows => "Windows (Wine)",
    };
    eprintln!("rbuild: building the {kind} build image on the remote (first time only)…");

    let context = build_context(target)?;
    // `docker build -t <img> --label … -` reads a tar context from stdin.
    let build_cmd = format!("{docker} build -t {image} --label {LABEL} -");
    let mut child = ssh_spawn(remote, &build_cmd)?;
    {
        let mut stdin = child.stdin.take().context("ssh stdin missing")?;
        stdin.write_all(&context).await?;
        stdin.flush().await?;
    }
    let status = child.wait().await?;
    if !status.success() {
        anyhow::bail!("building {image} on the remote failed");
    }
    Ok(())
}

async fn image_present(remote: &RemoteConfig, docker: &str, image: &str) -> Result<bool> {
    let script = format!("{docker} image inspect {image} >/dev/null 2>&1 && echo yes || echo no");
    let mut cmd = ssh_base(remote);
    let out = cmd.arg(script).output().await.context("running ssh")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim() == "yes")
}

/// Builds an uncompressed tar of the Docker build context for `target`. Docker
/// accepts a raw tar on stdin; the Dockerfile must be named `Dockerfile`.
fn build_context(target: Target) -> Result<Vec<u8>> {
    let mut files: Vec<(&str, &str, bool)> = Vec::new();
    match target {
        Target::Linux => {
            files.push(("Dockerfile", LINUX_DOCKERFILE, false));
        }
        Target::Windows => {
            files.push(("Dockerfile", WINE_DOCKERFILE, false));
            // The Wine image COPYs these wrappers; executable bit matters.
            files.push(("wine-wrappers/_common.sh", WINE_COMMON, true));
            files.push(("wine-wrappers/cargo", WINE_CARGO, true));
            files.push(("wine-wrappers/rustc", WINE_RUSTC, true));
        }
    }

    let mut tar = tar::Builder::new(Vec::new());
    for (path, contents, executable) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(if executable { 0o755 } else { 0o644 });
        header.set_cksum();
        tar.append_data(&mut header, path, contents.as_bytes())
            .with_context(|| format!("adding {path} to build context"))?;
    }
    tar.into_inner().context("finalizing build context tar")
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

fn ssh_spawn(remote: &RemoteConfig, script: &str) -> Result<tokio::process::Child> {
    ssh_base(remote)
        .arg(script)
        .stdin(Stdio::piped())
        // Build output (image layers) streams to the user's terminal via stderr.
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawning ssh for image build")
}
