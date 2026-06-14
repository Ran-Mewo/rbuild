//! Daemon-side build handling: run the requested command in a backend, stream
//! its output, then offer the produced artifacts back to the client.

use std::path::Path;

use anyhow::{Context, Result};
use rbuild_proto::proto::{BuildRequest, Message, OutputFd};
use rbuild_proto::scan;
use rbuild_proto::transport::Frame;
use tokio::sync::mpsc;

use crate::backend::{self, OutputSink};
use crate::serve::FrameSink;
use crate::sync::workspace_dir;

/// Output sink that forwards each backend output line to the client as a
/// BuildOutput frame.
struct BuildSink {
    stream: u32,
    out: FrameSink,
}

#[async_trait::async_trait]
impl OutputSink for BuildSink {
    async fn line(&mut self, fd: OutputFd, text: &str) -> Result<()> {
        self.out
            .control(self.stream, &Message::BuildOutput { stream_fd: fd, data: text.to_string() })
            .await
    }

    async fn started(&mut self, container: &str) -> Result<()> {
        self.out
            .control(self.stream, &Message::BuildStarted { container: container.to_string() })
            .await
    }
}

/// Runs a build start to finish on the given stream.
pub async fn run(
    stream: u32,
    workspace_id: &str,
    req: BuildRequest,
    linux_image: &str,
    wine_image: &str,
    out: &FrameSink,
    incoming_need: &mut mpsc::Receiver<Vec<String>>,
) -> Result<()> {
    let workspace = workspace_dir(workspace_id)?;
    if !workspace.exists() {
        anyhow::bail!("workspace {workspace_id} not synced yet");
    }

    // Build containers are launched as siblings via the Docker socket, so they
    // mount the same named volumes as the daemon (by name, not by host path —
    // the daemon's own /work and /cache mounts have no host-visible path).
    let ws_volume = crate::serve::ws_volume()
        .context("RBUILD_WS_VOLUME not set — daemon must run with a workspace volume")?;
    let cache_volume = crate::serve::cache_volume()
        .context("RBUILD_CACHE_VOLUME not set — daemon must run with a cache volume")?;

    // Snapshot the tree before the build so we can tell exactly what it
    // generated — every new or changed file is sent back, no globs to declare.
    let before = scan::scan_raw(&workspace)?;

    let backend = backend::for_target(req.target, linux_image, wine_image);
    let mut sink = BuildSink { stream, out: out.clone() };
    let exit_code = backend
        .run(&req, &ws_volume, &cache_volume, &mut sink)
        .await?;

    // Anything the build created or modified (content hash differs from before)
    // is an artifact. The client already has the unchanged files from the sync.
    let after = scan::scan_raw(&workspace)?;
    let entries: Vec<_> = after
        .values()
        .filter(|e| before.get(&e.path).map(|b| b.hash != e.hash).unwrap_or(true))
        .cloned()
        .collect();
    out.control(stream, &Message::ArtifactManifest { entries }).await?;

    if let Some(need) = incoming_need.recv().await {
        for rel in &need {
            send_artifact(stream, &workspace, rel, out).await?;
        }
    }

    out.control(stream, &Message::BuildFinished { exit_code }).await?;
    Ok(())
}

async fn send_artifact(
    stream: u32,
    workspace: &Path,
    rel: &str,
    out: &FrameSink,
) -> Result<()> {
    let abs = workspace.join(rel);
    let meta = std::fs::metadata(&abs)?;
    let data = std::fs::read(&abs)?;
    let compressed = rbuild_proto::chunk::compress(&data)?;
    let header = Message::FileHeader {
        path: rel.to_string(),
        len: meta.len(),
        // Preserve the real mode so executables come back runnable.
        mode: file_mode(&meta),
        compressed: true,
    };
    out.control(stream, &header).await?;
    out.send(Frame::data(stream, compressed)).await?;
    Ok(())
}

#[cfg(unix)]
fn file_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode()
}

#[cfg(not(unix))]
fn file_mode(_meta: &std::fs::Metadata) -> u32 {
    0o644
}
