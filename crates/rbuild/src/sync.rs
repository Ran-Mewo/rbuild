//! Client-side sync driver — three-way merge over a bidirectional protocol.
//!
//! The client sends its manifest, receives the daemon's, and merges both
//! against the ancestor snapshot from the last sync. It then pushes locally
//! changed files, pulls remotely changed ones, applies deletions on both
//! sides, and writes conflicts as sidecars (never overwriting). Small files go
//! whole (zstd); large files use a rolling-hash delta.

use anyhow::{Context, Result};
use rbuild_proto::proto::Message;
use rbuild_proto::scan::Manifest;
use rbuild_proto::transport::{read_frame, write_frame, Frame};
use rbuild_proto::{chunk, merge, scan};
use std::path::Path;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::ancestor;

/// Files at or below this size are compressed and sent in one data frame.
/// Larger files are sent in raw chunks so memory stays bounded.
const WHOLE_FILE_LIMIT: u64 = 4 * 1024 * 1024;
const RAW_CHUNK: usize = 1024 * 1024;

pub struct SyncStats {
    /// Files pushed to the remote.
    pub sent: usize,
    /// Files pulled from the remote.
    pub pulled: usize,
    /// Files deleted (either side).
    pub deleted: usize,
    /// Conflicts written as sidecars.
    pub conflicts: usize,
    /// Files the daemon reported applying.
    pub applied: u64,
}

/// Runs one full three-way sync round on the given logical stream.
pub async fn run<R, W>(
    stream: u32,
    workspace: &str,
    root: &Path,
    reader: &mut R,
    writer: &mut W,
) -> Result<SyncStats>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    write_frame(
        writer,
        &Frame::control(stream, &Message::OpenSync { stream, workspace: workspace.into() })?,
    )
    .await?;

    // Exchange manifests, then merge against the ancestor snapshot.
    let local = scan::scan_with_ignores(root).context("scanning code root")?;
    let entries: Vec<_> = local.values().cloned().collect();
    write_frame(writer, &Frame::control(stream, &Message::SyncManifest { entries })?).await?;

    let remote: Manifest = match next_message(reader).await? {
        Message::RemoteManifest { entries } => {
            entries.into_iter().map(|e| (e.path.clone(), e)).collect()
        }
        other => anyhow::bail!("expected RemoteManifest, got {other:?}"),
    };

    let anc = ancestor::load(workspace);
    let plan = merge::plan(&local, &anc, &remote);

    // Push locally-changed files.
    for rel in &plan.push {
        send_file(stream, root, rel, reader, writer).await?;
    }
    // Tell the daemon what to delete and what we want pulled back.
    if !plan.delete_remote.is_empty() {
        write_frame(
            writer,
            &Frame::control(stream, &Message::RemoteDelete { files: plan.delete_remote.clone() })?,
        )
        .await?;
    }
    // Pull = remote-changed files plus the remote copy of each conflict (saved
    // as a sidecar so the user can reconcile by hand).
    let mut pull = plan.pull.clone();
    pull.extend(plan.conflicts.iter().cloned());
    if !pull.is_empty() {
        write_frame(
            writer,
            &Frame::control(stream, &Message::PullRequest { files: pull.clone() })?,
        )
        .await?;
    }
    write_frame(writer, &Frame::control(stream, &Message::SyncComplete)?).await?;

    // Apply local deletions before writing pulled files.
    for rel in &plan.delete_local {
        let _ = std::fs::remove_file(root.join(rel));
    }

    // Receive pulled files (and conflict sidecars) until SyncAck.
    let conflict_set: std::collections::BTreeSet<&String> = plan.conflicts.iter().collect();
    let applied = receive_until_ack(stream, root, reader, &conflict_set).await?;

    // Record the new ancestor: the state both sides now agree on.
    let next = ancestor::next_ancestor(
        &local,
        &remote,
        &plan.pull,
        &plan.delete_local,
        &plan.conflicts,
    );
    ancestor::save(workspace, &next)?;

    Ok(SyncStats {
        sent: plan.push.len(),
        pulled: plan.pull.len(),
        deleted: plan.delete_local.len() + plan.delete_remote.len(),
        conflicts: plan.conflicts.len(),
        applied,
    })
}

/// Receives daemon→client file content (pulls and conflict sidecars) until the
/// daemon sends SyncAck. Conflict files are written to a sidecar path so the
/// local copy is never overwritten.
async fn receive_until_ack<R>(
    _stream: u32,
    root: &Path,
    reader: &mut R,
    conflicts: &std::collections::BTreeSet<&String>,
) -> Result<u64>
where
    R: AsyncRead + Unpin,
{
    let mut incoming: Option<IncomingFile> = None;
    loop {
        let frame = read_frame(reader).await?.context("connection closed during pull")?;
        if frame.kind == rbuild_proto::transport::FrameKind::Data {
            if let Some(f) = incoming.as_mut() {
                f.buf.extend_from_slice(&frame.payload);
                if f.complete() {
                    let f = incoming.take().unwrap();
                    f.write(root, conflicts)?;
                }
            }
            continue;
        }
        match frame.as_message()? {
            Message::FileHeader { path, len, mode, compressed } => {
                incoming = Some(IncomingFile { path, len, mode, compressed, buf: Vec::new() });
            }
            Message::SyncAck { applied, .. } => return Ok(applied),
            Message::Error { message } => anyhow::bail!("daemon error: {message}"),
            other => anyhow::bail!("unexpected message during pull: {other:?}"),
        }
    }
}

struct IncomingFile {
    path: String,
    len: u64,
    mode: u32,
    compressed: bool,
    buf: Vec<u8>,
}

impl IncomingFile {
    fn complete(&self) -> bool {
        // Compressed payloads arrive in a single data frame; raw ones fill len.
        if self.compressed {
            true
        } else {
            self.buf.len() as u64 >= self.len
        }
    }

    fn write(self, root: &Path, conflicts: &std::collections::BTreeSet<&String>) -> Result<()> {
        let data = if self.compressed {
            chunk::decompress(&self.buf)?
        } else {
            self.buf
        };
        // A conflict's remote copy is written beside the local file, never over
        // it, so no work is lost; the user reconciles the two by hand.
        let rel = if conflicts.contains(&self.path) {
            format!("{}.rbuild-conflict-{}", self.path, hostname())
        } else {
            self.path.clone()
        };
        let abs = root.join(&rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&abs, &data)?;
        set_mode(&abs, self.mode)?;
        if conflicts.contains(&self.path) {
            eprintln!("rbuild: conflict on {} — remote copy saved as {}", self.path, rel);
        }
        Ok(())
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "remote".to_string())
}

/// Threshold above which we attempt a rolling-hash delta against the daemon's
/// existing copy instead of resending the whole file. Below it the round-trip
/// to fetch a signature costs more than just sending the (compressed) bytes.
const DELTA_THRESHOLD: u64 = 1024 * 1024;

async fn send_file<R, W>(
    stream: u32,
    root: &Path,
    rel: &str,
    reader: &mut R,
    writer: &mut W,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let abs = root.join(rel);
    let meta = std::fs::metadata(&abs)
        .with_context(|| format!("stat {}", abs.display()))?;
    let mode = file_mode(&meta);
    let len = meta.len();

    if len <= WHOLE_FILE_LIMIT {
        let data = std::fs::read(&abs)?;
        let compressed = chunk::compress(&data)?;
        write_frame(
            writer,
            &Frame::control(
                stream,
                &Message::FileHeader { path: rel.into(), len, mode, compressed: true },
            )?,
        )
        .await?;
        write_frame(writer, &Frame::data(stream, compressed)).await?;
        return Ok(());
    }

    if len >= DELTA_THRESHOLD {
        // Ask the daemon for its current chunk signature; if it has a copy we
        // send only the changed chunks.
        write_frame(
            writer,
            &Frame::control(stream, &Message::DeltaRequest { path: rel.into() })?,
        )
        .await?;
        let base_sig = match next_message(reader).await? {
            Message::DeltaSignature { chunks, .. } => chunks,
            other => anyhow::bail!("expected DeltaSignature, got {other:?}"),
        };
        if !base_sig.is_empty() {
            let data = std::fs::read(&abs)?;
            let (ops, literals) = chunk::make_delta(&data, &base_sig);
            write_frame(
                writer,
                &Frame::control(
                    stream,
                    &Message::DeltaInstructions { path: rel.into(), len, mode, ops },
                )?,
            )
            .await?;
            if !literals.is_empty() {
                write_frame(writer, &Frame::data(stream, chunk::compress(&literals)?)).await?;
            }
            write_frame(writer, &Frame::control(stream, &Message::DeltaEnd { path: rel.into() })?)
                .await?;
            return Ok(());
        }
        // No base copy on the daemon — fall through to a full streamed send.
    }

    // Large file with no usable base: header then raw chunks, no whole-file buffering.
    write_frame(
        writer,
        &Frame::control(
            stream,
            &Message::FileHeader { path: rel.into(), len, mode, compressed: false },
        )?,
    )
    .await?;
    use std::io::Read;
    let mut f = std::fs::File::open(&abs)?;
    let mut buf = vec![0u8; RAW_CHUNK];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        write_frame(writer, &Frame::data(stream, buf[..n].to_vec())).await?;
    }
    Ok(())
}

async fn next_message<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Message> {
    let frame = read_frame(reader)
        .await?
        .context("connection closed during sync")?;
    let msg = frame.as_message()?;
    if let Message::Error { message } = &msg {
        anyhow::bail!("daemon error: {message}");
    }
    Ok(msg)
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
