//! Daemon-side sync: receive a client manifest, request only the files that
//! differ, write incoming content into the workspace mirror, and apply
//! deletions. All workspaces live under the single rbuildd root.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rbuild_proto::chunk;
use rbuild_proto::proto::Message;
use rbuild_proto::scan;
use rbuild_proto::transport::Frame;

use crate::serve::{workspace_root, FrameSink};

/// The workspace mirror directory. With the containerized daemon the workspace
/// volume is mounted directly at the work root, so each daemon instance serves
/// exactly one workspace; the id is validated only as a safety check on the
/// value the client sent.
pub fn workspace_dir(workspace_id: &str) -> Result<PathBuf> {
    if workspace_id.is_empty()
        || workspace_id.contains('/')
        || workspace_id.contains('\\')
        || workspace_id.contains("..")
    {
        anyhow::bail!("invalid workspace id {workspace_id:?}");
    }
    Ok(workspace_root())
}

/// State for one in-progress sync round on a stream.
pub struct SyncSession {
    stream: u32,
    root: PathBuf,
    /// File currently being received, set by a FileHeader.
    incoming: Option<Incoming>,
    /// Delta currently being received, set by DeltaInstructions.
    incoming_delta: Option<IncomingDelta>,
    /// Files the client asked to delete from the mirror (applied at completion).
    pending_delete: Vec<String>,
    /// Files the client asked to pull back (streamed at completion).
    pending_pull: Vec<String>,
    applied: u64,
}

struct Incoming {
    path: String,
    len: u64,
    mode: u32,
    compressed: bool,
    buf: Vec<u8>,
}

/// A delta currently being received for one large file.
struct IncomingDelta {
    path: String,
    len: u64,
    mode: u32,
    ops: Vec<rbuild_proto::proto::DeltaOpKind>,
    /// Compressed literal bytes accumulated from data frames.
    literals: Vec<u8>,
}

impl SyncSession {
    pub fn open(stream: u32, workspace_id: &str) -> Result<Self> {
        let root = workspace_dir(workspace_id)?;
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating workspace dir {}", root.display()))?;
        Ok(SyncSession {
            stream,
            root,
            incoming: None,
            incoming_delta: None,
            pending_delete: Vec::new(),
            pending_pull: Vec::new(),
            applied: 0,
        })
    }

    /// Reply to the client's manifest with the daemon's own current manifest,
    /// so the client can compute the three-way merge.
    pub async fn on_manifest(&self, out: &FrameSink) -> Result<()> {
        let remote = scan::scan_raw(&self.root)?;
        let entries: Vec<_> = remote.values().cloned().collect();
        tracing::info!(files = entries.len(), "sent remote manifest");
        out.control(self.stream, &Message::RemoteManifest { entries }).await
    }

    /// Record files the client wants deleted from the mirror.
    pub fn on_remote_delete(&mut self, files: Vec<String>) {
        self.pending_delete = files;
    }

    /// Record files the client wants streamed back.
    pub fn on_pull_request(&mut self, files: Vec<String>) {
        self.pending_pull = files;
    }

    pub fn on_file_header(&mut self, path: String, len: u64, mode: u32, compressed: bool) {
        self.incoming = Some(Incoming {
            path,
            len,
            mode,
            compressed,
            buf: Vec::with_capacity(len.min(1 << 20) as usize),
        });
    }

    /// Append a data frame's bytes to whatever transfer is in progress —
    /// either a whole-file receive or a delta's literal payload.
    pub fn on_data(&mut self, bytes: &[u8]) -> Result<()> {
        if let Some(delta) = self.incoming_delta.as_mut() {
            delta.literals.extend_from_slice(bytes);
            return Ok(());
        }
        let done = {
            let inc = self
                .incoming
                .as_mut()
                .context("data frame with no FileHeader")?;
            inc.buf.extend_from_slice(bytes);
            let expected = if inc.compressed { None } else { Some(inc.len) };
            match expected {
                Some(n) => inc.buf.len() as u64 >= n,
                // Compressed payloads are sent as a single data frame.
                None => true,
            }
        };
        if done {
            self.flush_incoming()?;
        }
        Ok(())
    }

    /// Reply to a DeltaRequest with the chunk signature of the mirror's current
    /// copy (empty if absent), so the client can send only changed chunks.
    pub async fn on_delta_request(
        &self,
        path: String,
        out: &FrameSink,
    ) -> Result<()> {
        let abs = safe_join(&self.root, &path)?;
        let chunks = match std::fs::read(&abs) {
            Ok(data) => chunk::signature(&data),
            Err(_) => Vec::new(),
        };
        out.control(self.stream, &Message::DeltaSignature { path, chunks }).await
    }

    pub fn on_delta_instructions(
        &mut self,
        path: String,
        len: u64,
        mode: u32,
        ops: Vec<rbuild_proto::proto::DeltaOpKind>,
    ) {
        self.incoming_delta = Some(IncomingDelta {
            path,
            len,
            mode,
            ops,
            literals: Vec::new(),
        });
    }

    /// Rebuild the file from the daemon's existing copy plus the received delta.
    pub fn on_delta_end(&mut self) -> Result<()> {
        let delta = self.incoming_delta.take().context("DeltaEnd with no delta")?;
        let abs = safe_join(&self.root, &delta.path)?;
        let base = std::fs::read(&abs).unwrap_or_default();
        let literals = if delta.literals.is_empty() {
            Vec::new()
        } else {
            chunk::decompress(&delta.literals)?
        };
        let rebuilt = chunk::apply_delta(&base, &delta.ops, &literals)?;
        if rebuilt.len() as u64 != delta.len {
            anyhow::bail!(
                "delta for {} rebuilt {} bytes, expected {}",
                delta.path,
                rebuilt.len(),
                delta.len
            );
        }
        std::fs::write(&abs, &rebuilt)
            .with_context(|| format!("writing {}", abs.display()))?;
        set_mode(&abs, delta.mode)?;
        self.applied += 1;
        Ok(())
    }

    fn flush_incoming(&mut self) -> Result<()> {
        let inc = self.incoming.take().context("flush with no incoming")?;
        let data = if inc.compressed {
            chunk::decompress(&inc.buf)?
        } else {
            inc.buf
        };
        let abs = safe_join(&self.root, &inc.path)?;
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&abs, &data)
            .with_context(|| format!("writing {}", abs.display()))?;
        set_mode(&abs, inc.mode)?;
        self.applied += 1;
        Ok(())
    }

    /// Apply the round: delete the requested files, stream back the requested
    /// pulls, then acknowledge.
    pub async fn complete(&mut self, out: &FrameSink) -> Result<()> {
        let mut deleted = 0u64;
        let to_delete = std::mem::take(&mut self.pending_delete);
        for rel in &to_delete {
            let abs = safe_join(&self.root, rel)?;
            if abs.is_file() {
                std::fs::remove_file(&abs).ok();
                deleted += 1;
            }
        }
        prune_empty_dirs(&self.root);

        // Stream pulled files back to the client (whole-file, zstd-compressed).
        let to_pull = std::mem::take(&mut self.pending_pull);
        for rel in &to_pull {
            self.send_file(rel, out).await?;
        }

        out.control(self.stream, &Message::SyncAck { applied: self.applied, deleted })
            .await?;
        tracing::info!(applied = self.applied, deleted, pulled = to_pull.len(), "sync complete");
        Ok(())
    }

    /// Streams one mirror file to the client as FileHeader + compressed data.
    async fn send_file(&self, rel: &str, out: &FrameSink) -> Result<()> {
        let abs = safe_join(&self.root, rel)?;
        let Ok(meta) = std::fs::metadata(&abs) else {
            return Ok(()); // gone between manifest and pull; client will reconcile
        };
        let data = std::fs::read(&abs)?;
        let compressed = chunk::compress(&data)?;
        out.control(
            self.stream,
            &Message::FileHeader {
                path: rel.to_string(),
                len: meta.len(),
                mode: file_mode(&meta),
                compressed: true,
            },
        )
        .await?;
        out.send(Frame::data(self.stream, compressed)).await
    }
}

/// Joins a relative path to root, refusing anything that would escape root.
fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    let mut out = root.to_path_buf();
    for comp in rel.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            anyhow::bail!("path {rel:?} escapes workspace");
        }
        out.push(comp);
    }
    Ok(out)
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

#[cfg(unix)]
fn file_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode()
}

#[cfg(not(unix))]
fn file_mode(_meta: &std::fs::Metadata) -> u32 {
    0o644
}

fn prune_empty_dirs(root: &Path) {
    fn recurse(dir: &Path, root: &Path) -> bool {
        let mut empty = true;
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if recurse(&path, root) {
                        std::fs::remove_dir(&path).ok();
                    } else {
                        empty = false;
                    }
                } else {
                    empty = false;
                }
            }
        }
        empty && dir != root
    }
    recurse(root, root);
}
