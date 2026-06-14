//! The serve loop.
//!
//! A single task reads frames from stdin; a single writer task owns stdout and
//! drains a frame channel. Sync runs inline (it's a tight request/response),
//! while each build runs on its own task that can independently await a later
//! `ArtifactNeed` from the client without blocking the reader — this is what
//! lets sync and build I/O share the one SSH byte stream.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use rbuild_proto::proto::{Message, PROTOCOL_VERSION};
use rbuild_proto::transport::{read_frame, write_frame, Frame, FrameKind};
use rbuild_proto::VERSION;
use tokio::io::{stdin, stdout};
use tokio::sync::mpsc;

use crate::build;
use crate::sync::SyncSession;

/// A handle for sending outbound frames. Both sync (inline) and build (its own
/// task) push frames here; the single writer task serializes them to stdout in
/// arrival order, so the two share the one byte stream without interleaving
/// within a frame.
#[derive(Clone)]
pub struct FrameSink {
    tx: mpsc::Sender<Frame>,
}

impl FrameSink {
    pub async fn send(&self, frame: Frame) -> Result<()> {
        self.tx
            .send(frame)
            .await
            .map_err(|_| anyhow::anyhow!("output channel closed"))
    }

    /// Convenience for the common case of a control message.
    pub async fn control(&self, stream: u32, msg: &Message) -> Result<()> {
        self.send(Frame::control(stream, msg)?).await
    }
}

/// The workspace mirror inside the daemon container. The client mounts the
/// workspace's Docker volume here, so the mirror lives entirely in that volume
/// — nothing touches the remote host filesystem. Overridable for tests.
pub fn workspace_root() -> PathBuf {
    std::env::var("RBUILD_WORK")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/work"))
}

/// Names of the Docker volumes the daemon was launched with, so it can mount
/// the same workspace and cache volumes into sibling build containers.
pub fn ws_volume() -> Option<String> {
    std::env::var("RBUILD_WS_VOLUME").ok()
}

pub fn cache_volume() -> Option<String> {
    std::env::var("RBUILD_CACHE_VOLUME").ok()
}

enum Stream {
    // Boxed because a SyncSession is much larger than the Build variant;
    // boxing keeps the enum small without changing behavior.
    Sync {
        session: Box<SyncSession>,
    },
    /// A running build, with a channel to deliver its ArtifactNeed reply.
    Build {
        need_tx: mpsc::Sender<Vec<String>>,
    },
}

pub async fn serve() -> Result<()> {
    let mut input = stdin();
    let mut output = stdout();
    handshake(&mut input, &mut output).await?;
    tracing::info!("client handshake complete");

    // All outbound frames funnel through this channel to the writer task, so
    // build tasks and the reader can emit frames without sharing stdout.
    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(256);
    let writer = tokio::spawn(async move {
        let mut output = output;
        while let Some(frame) = out_rx.recv().await {
            if write_frame(&mut output, &frame).await.is_err() {
                break;
            }
        }
    });
    let sink = FrameSink { tx: out_tx.clone() };

    let mut streams: HashMap<u32, Stream> = HashMap::new();

    while let Some(frame) = read_frame(&mut input).await? {
        match frame.kind {
            FrameKind::Data => {
                if let Some(Stream::Sync { session, .. }) = streams.get_mut(&frame.stream) {
                    session.on_data(&frame.payload)?;
                }
            }
            FrameKind::Close => {
                streams.remove(&frame.stream);
            }
            FrameKind::Control => {
                let msg = frame.as_message()?;
                if let Err(e) = handle_control(frame.stream, msg, &mut streams, &sink).await {
                    tracing::warn!(error = %e, "stream error");
                    sink.control(frame.stream, &Message::Error { message: e.to_string() })
                        .await
                        .ok();
                }
            }
        }
    }

    drop(out_tx);
    drop(sink);
    let _ = writer.await;
    Ok(())
}

async fn handle_control(
    stream: u32,
    msg: Message,
    streams: &mut HashMap<u32, Stream>,
    sink: &FrameSink,
) -> Result<()> {
    match msg {
        Message::OpenSync { stream: s, workspace } => {
            let session = SyncSession::open(s, &workspace)?;
            streams.insert(s, Stream::Sync { session: Box::new(session) });
        }
        Message::SyncManifest { .. } => {
            let Some(Stream::Sync { session }) = streams.get_mut(&stream) else {
                anyhow::bail!("SyncManifest on non-sync stream {stream}");
            };
            // The client's manifest is informational; we reply with our own so
            // the client can run the three-way merge.
            session.on_manifest(sink).await?;
        }
        Message::FileHeader { path, len, mode, compressed } => {
            let Some(Stream::Sync { session }) = streams.get_mut(&stream) else {
                anyhow::bail!("FileHeader on non-sync stream {stream}");
            };
            session.on_file_header(path, len, mode, compressed);
        }
        Message::DeltaRequest { path } => {
            let Some(Stream::Sync { session }) = streams.get_mut(&stream) else {
                anyhow::bail!("DeltaRequest on non-sync stream {stream}");
            };
            session.on_delta_request(path, sink).await?;
        }
        Message::DeltaInstructions { path, len, mode, ops } => {
            let Some(Stream::Sync { session }) = streams.get_mut(&stream) else {
                anyhow::bail!("DeltaInstructions on non-sync stream {stream}");
            };
            session.on_delta_instructions(path, len, mode, ops);
        }
        Message::DeltaEnd { .. } => {
            let Some(Stream::Sync { session }) = streams.get_mut(&stream) else {
                anyhow::bail!("DeltaEnd on non-sync stream {stream}");
            };
            session.on_delta_end()?;
        }
        Message::RemoteDelete { files } => {
            let Some(Stream::Sync { session }) = streams.get_mut(&stream) else {
                anyhow::bail!("RemoteDelete on non-sync stream {stream}");
            };
            session.on_remote_delete(files);
        }
        Message::PullRequest { files } => {
            let Some(Stream::Sync { session }) = streams.get_mut(&stream) else {
                anyhow::bail!("PullRequest on non-sync stream {stream}");
            };
            session.on_pull_request(files);
        }
        Message::SyncComplete => {
            let Some(Stream::Sync { session }) = streams.get_mut(&stream) else {
                anyhow::bail!("SyncComplete on non-sync stream {stream}");
            };
            session.complete(sink).await?;
        }
        Message::OpenBuild {
            stream: s,
            workspace,
            request,
            artifact_globs,
            linux_image,
            wine_image,
        } => {
            let (need_tx, mut need_rx) = mpsc::channel::<Vec<String>>(1);
            streams.insert(s, Stream::Build { need_tx });
            let sink = sink.clone();
            // Run the build off the reader task so it can await ArtifactNeed.
            tokio::spawn(async move {
                if let Err(e) = build::run(
                    s,
                    &workspace,
                    request,
                    &artifact_globs,
                    &linux_image,
                    &wine_image,
                    &sink,
                    &mut need_rx,
                )
                .await
                {
                    sink.control(s, &Message::Error { message: e.to_string() }).await.ok();
                }
            });
        }
        Message::ArtifactNeed { files } => {
            if let Some(Stream::Build { need_tx }) = streams.get(&stream) {
                need_tx.send(files).await.ok();
            }
        }
        other => anyhow::bail!("unexpected control message: {other:?}"),
    }
    Ok(())
}

async fn handshake(
    input: &mut tokio::io::Stdin,
    output: &mut tokio::io::Stdout,
) -> Result<()> {
    let frame = read_frame(input)
        .await?
        .context("client closed before handshake")?;
    let ok = matches!(
        frame.as_message()?,
        Message::Hello { protocol, .. } if protocol == PROTOCOL_VERSION
    );
    let welcome = Message::Welcome {
        protocol: PROTOCOL_VERSION,
        daemon_version: VERSION.to_string(),
        ok,
        message: if ok { "ok".into() } else { "protocol version mismatch".into() },
    };
    write_frame(output, &Frame::control(0, &welcome)?).await?;
    if !ok {
        anyhow::bail!("handshake rejected");
    }
    Ok(())
}
