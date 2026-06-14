//! Docker-based build backend.
//!
//! The workspace mirror is bind-mounted into a container that carries the
//! toolchains, so nothing is installed on the host and the host filesystem
//! stays pristine. The same backend serves Linux builds (run the command
//! directly) and Windows builds (wrap the command in Wine) — only the image
//! and command prefix differ.

use std::process::Stdio;

use anyhow::{Context, Result};
use rbuild_proto::proto::{BuildRequest, OutputFd};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use super::{BuildBackend, OutputSink};

/// Where the workspace is mounted inside the container.
const WORKDIR: &str = "/work";

pub struct DockerBackend {
    image: String,
    /// Windows builds run inside a persistent Wine prefix. When set, the prefix
    /// lives under the build cache so it survives across builds (wine init is
    /// slow), and the image's PATH wrappers route each command through Wine.
    wine: bool,
}

impl DockerBackend {
    pub fn linux(image: &str) -> Self {
        DockerBackend {
            image: image.to_string(),
            wine: false,
        }
    }

    pub fn wine(image: &str) -> Self {
        DockerBackend {
            image: image.to_string(),
            wine: true,
        }
    }
}

#[async_trait::async_trait]
impl BuildBackend for DockerBackend {
    async fn run(
        &self,
        req: &BuildRequest,
        ws_volume: &str,
        cache_volume: &str,
        sink: &mut dyn OutputSink,
    ) -> Result<i32> {
        // The command runs in the subdirectory the user launched it from,
        // resolved relative to the mounted workspace root.
        let rel_cwd = if req.cwd.is_empty() || req.cwd == "." {
            String::new()
        } else {
            req.cwd.clone()
        };
        let container_cwd = if rel_cwd.is_empty() {
            WORKDIR.to_string()
        } else {
            format!("{WORKDIR}/{rel_cwd}")
        };

        let mut docker = Command::new("docker");
        docker
            .arg("run")
            .arg("--rm")
            // Keep stdin closed; builds are non-interactive.
            .arg("-i")
            .arg("-w")
            .arg(&container_cwd)
            // Mount the same named volumes the daemon holds, so the build sees
            // the synced tree and shares the persistent cache. No host paths.
            .arg("-v")
            .arg(format!("{ws_volume}:{WORKDIR}"));

        // Cache volume mounted as the build user's HOME, separate from the
        // workspace so it never appears in the synced tree.
        const CACHE_MNT: &str = "/rbuild-home";
        docker
            .arg("-v")
            .arg(format!("{cache_volume}:{CACHE_MNT}"))
            .arg("-e")
            .arg(format!("HOME={CACHE_MNT}"))
            .arg("-e")
            .arg(format!("CARGO_HOME={CACHE_MNT}/cargo"));

        if self.wine {
            // Persist the Wine prefix in the cache volume so the slow first-run
            // initialization happens once. The image's PATH wrappers route the
            // user's command through Wine, so argv is passed through unchanged —
            // every build command "just works" as if on Windows.
            docker
                .arg("-e")
                .arg(format!("WINEPREFIX={CACHE_MNT}/wineprefix"))
                .arg("-e")
                .arg("WINEDEBUG=-all");
        }

        // Build runs as root inside the container. Because all state lives in
        // Docker volumes (never the host FS), root here leaves no host-visible
        // files, and a uniform uid across the daemon and build containers means
        // the daemon can always read back the artifacts the build produced.

        for (k, v) in &req.env {
            docker.arg("-e").arg(format!("{k}={v}"));
        }

        docker.arg(&self.image);
        docker.args(&req.argv);

        docker
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        sink.started(&self.image).await?;

        let mut child = docker
            .spawn()
            .context("spawning docker — is Docker installed and running on the remote?")?;

        let stdout = child.stdout.take().context("docker stdout missing")?;
        let stderr = child.stderr.take().context("docker stderr missing")?;
        let mut out_lines = BufReader::new(stdout).lines();
        let mut err_lines = BufReader::new(stderr).lines();

        // Interleave stdout and stderr as lines arrive, forwarding each to the
        // client so output appears live.
        loop {
            tokio::select! {
                line = out_lines.next_line() => match line? {
                    Some(l) => sink.line(OutputFd::Stdout, &l).await?,
                    None => break,
                },
                line = err_lines.next_line() => {
                    if let Some(l) = line? {
                        sink.line(OutputFd::Stderr, &l).await?;
                    }
                },
            }
        }
        // Drain any remaining stderr after stdout closed.
        while let Some(l) = err_lines.next_line().await? {
            sink.line(OutputFd::Stderr, &l).await?;
        }

        let status = child.wait().await?;
        Ok(status.code().unwrap_or(-1))
    }
}
