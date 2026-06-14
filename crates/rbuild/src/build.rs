//! Client-side build dispatch: send a build request, render its output live,
//! and pull back the artifacts it produced.

use std::path::Path;

use anyhow::{Context, Result};
use rbuild_proto::chunk;
use rbuild_proto::config::GlobalConfig;
use rbuild_proto::proto::{BuildRequest, Message, OutputFd, Target};
use rbuild_proto::scan;
use rbuild_proto::transport::{read_frame, write_frame, Frame};
use std::io::Write;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::connection::Connection;

/// Dispatches a build to the remote and mirrors its result locally. The build
/// runs in `rel_cwd` (relative to the code-root mirror); artifacts under that
/// directory are synced back. Returns the remote process's exit code.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch(
    conn: &mut Connection,
    cfg: &GlobalConfig,
    workspace_id: &str,
    local_root: &Path,
    argv: Vec<String>,
    rel_cwd: String,
    target: Target,
) -> Result<i32> {
    let stream = 2;
    let request = BuildRequest {
        argv,
        cwd: rel_cwd,
        target,
        env: forwarded_env(),
    };
    let open = Message::OpenBuild {
        stream,
        workspace: workspace_id.to_string(),
        request,
        artifact_globs: cfg.build.artifacts.clone(),
        linux_image: cfg.build.linux_image.clone(),
        wine_image: cfg.build.wine_image.clone(),
    };
    write_frame(&mut conn.stdin, &Frame::control(stream, &open)?).await?;

    drive(stream, local_root, &mut conn.stdout, &mut conn.stdin).await
}

/// Reads build frames until BuildFinished, printing output and handling the
/// artifact exchange.
async fn drive<R, W>(stream: u32, root: &Path, reader: &mut R, writer: &mut W) -> Result<i32>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut incoming: Option<IncomingArtifact> = None;
    loop {
        let frame = read_frame(reader)
            .await?
            .context("connection closed during build")?;

        // Data frames carry artifact content for the file named by the last header.
        if rbuild_proto::transport::FrameKind::Data == frame.kind {
            if let Some(art) = incoming.take() {
                write_artifact(root, &art, &frame.payload)?;
            }
            continue;
        }

        match frame.as_message()? {
            Message::BuildStarted { container } => {
                tracing::info!(%container, "build started");
            }
            Message::BuildOutput { stream_fd, data } => match stream_fd {
                OutputFd::Stdout => {
                    let mut out = std::io::stdout();
                    let _ = writeln!(out, "{data}");
                }
                OutputFd::Stderr => {
                    let mut err = std::io::stderr();
                    let _ = writeln!(err, "{data}");
                }
            },
            Message::ArtifactManifest { entries } => {
                let need = artifacts_to_pull(root, &entries);
                write_frame(writer, &Frame::control(stream, &Message::ArtifactNeed { files: need })?)
                    .await?;
            }
            Message::FileHeader { path, len, mode, compressed } => {
                incoming = Some(IncomingArtifact { path, len, mode, compressed });
            }
            Message::BuildFinished { exit_code } => return Ok(exit_code),
            Message::Error { message } => anyhow::bail!("remote build error: {message}"),
            other => anyhow::bail!("unexpected build message: {other:?}"),
        }
    }
}

struct IncomingArtifact {
    path: String,
    len: u64,
    mode: u32,
    compressed: bool,
}

/// Compares the remote artifact manifest against local copies and returns the
/// paths whose content differs (or is absent) locally — only those transfer.
fn artifacts_to_pull(root: &Path, entries: &[rbuild_proto::proto::ManifestEntry]) -> Vec<String> {
    let mut need = Vec::new();
    for e in entries {
        let local = root.join(&e.path);
        let same = std::fs::read(&local)
            .map(|d| scan::content_hash(&d) == e.hash)
            .unwrap_or(false);
        if !same {
            need.push(e.path.clone());
        }
    }
    need
}

fn write_artifact(root: &Path, art: &IncomingArtifact, payload: &[u8]) -> Result<()> {
    let data = if art.compressed {
        chunk::decompress(payload)?
    } else {
        payload.to_vec()
    };
    let abs = root.join(&art.path);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&abs, &data)?;
    set_mode(&abs, art.mode)?;
    let _ = art.len;
    Ok(())
}

/// Build tools rely on a handful of environment variables; forward the safe,
/// build-relevant ones rather than the client's entire environment.
fn forwarded_env() -> Vec<(String, String)> {
    const KEEP: &[&str] = &["CARGO_TERM_COLOR", "RUST_BACKTRACE", "RUSTFLAGS", "CC", "CXX"];
    KEEP.iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect()
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if mode != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}
