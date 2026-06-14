//! Wire protocol messages exchanged as JSON-encoded control frames.

use serde::{Deserialize, Serialize};

use crate::hash::Hash;

/// Bumped on any breaking change to the message set. The handshake refuses a
/// mismatch rather than risk a silently corrupt session.
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", content = "c")]
pub enum Message {
    /// First message the client sends after the daemon process starts.
    Hello {
        protocol: u32,
        client_version: String,
    },
    /// Daemon's reply. `ok = false` means the client must abort.
    Welcome {
        protocol: u32,
        daemon_version: String,
        ok: bool,
        message: String,
    },

    /// Open a logical stream for a workspace sync session.
    OpenSync { stream: u32, workspace: String },
    /// Open a logical stream to run a build. Carries the project settings the
    /// daemon needs (the daemon holds no per-project config of its own).
    OpenBuild {
        stream: u32,
        workspace: String,
        request: BuildRequest,
        artifact_globs: Vec<String>,
        linux_image: String,
        wine_image: String,
    },

    /// Client announces its full file manifest. The daemon replies with its own
    /// (`RemoteManifest`) so the client can compute a three-way merge.
    SyncManifest { entries: Vec<ManifestEntry> },
    /// Daemon's current mirror manifest, sent in response to `SyncManifest`.
    RemoteManifest { entries: Vec<ManifestEntry> },
    /// Header preceding the data frames for one file's content (either
    /// direction: client→daemon for a push, daemon→client for a pull).
    FileHeader {
        path: String,
        len: u64,
        mode: u32,
        compressed: bool,
    },
    /// For a large changed file, the client asks the daemon for the chunk
    /// signature of its current mirror copy so it can send only the diff.
    DeltaRequest { path: String },
    /// Daemon's reply: the ordered chunk signature of its copy (empty if it
    /// has no copy). The client matches its own chunks against these.
    DeltaSignature { path: String, chunks: Vec<crate::chunk::ChunkRef> },
    /// Client's reconstruction plan for `path`. The literal bytes referenced by
    /// `Data` ops follow as data frames (concatenated in op order); `DeltaEnd`
    /// marks the end so the daemon can rebuild and write the file.
    DeltaInstructions {
        path: String,
        len: u64,
        mode: u32,
        ops: Vec<DeltaOpKind>,
    },
    DeltaEnd { path: String },
    /// Client tells the daemon which mirror files to delete (resolved by the
    /// merge as "deleted locally, unchanged remotely").
    RemoteDelete { files: Vec<String> },
    /// Client asks the daemon to send back these files (resolved by the merge as
    /// "changed remotely, take remote"). The daemon streams them as
    /// FileHeader+data after `SyncComplete`.
    PullRequest { files: Vec<String> },
    /// All client→daemon content and the delete/pull requests have been sent.
    SyncComplete,
    /// Daemon confirms the round: how many files it wrote and deleted, sent
    /// after it has streamed any pulled files back.
    SyncAck { applied: u64, deleted: u64 },

    /// Build lifecycle.
    BuildStarted { container: String },
    BuildOutput { stream_fd: OutputFd, data: String },
    /// After the process exits, the daemon advertises the artifacts it produced
    /// (files under the workspace matching the project's artifact globs) so the
    /// client can pull back only the ones that changed.
    ArtifactManifest { entries: Vec<ManifestEntry> },
    /// Client's reply: the artifact paths it actually needs transferred.
    ArtifactNeed { files: Vec<String> },
    BuildFinished { exit_code: i32 },

    /// Sent on either side when something goes wrong on a stream.
    Error { message: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum OutputFd {
    Stdout,
    Stderr,
}

/// A single delta reconstruction step. `Copy` reuses a run of bytes already
/// present in the daemon's mirror copy; `Data` carries literal new bytes that
/// follow in a data frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeltaOpKind {
    /// Copy `len` bytes from the daemon's old file starting at `offset`.
    Copy { offset: u64, len: u32 },
    /// `len` literal bytes follow in the next data frame on this stream.
    Data { len: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub path: String,
    pub hash: Hash,
    pub len: u64,
    pub mode: u32,
}

/// What target a build should produce — drives backend selection on the daemon.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    #[default]
    Linux,
    Windows,
}

impl Target {
    /// The target matching the machine rbuild runs on, so a build defaults to
    /// producing artifacts for the user's own OS.
    pub fn host() -> Target {
        if cfg!(windows) {
            Target::Windows
        } else {
            Target::Linux
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildRequest {
    /// argv as the user typed it, e.g. `["cargo", "build", "--release"]`.
    pub argv: Vec<String>,
    /// Directory relative to the workspace root the command was launched in.
    pub cwd: String,
    pub target: Target,
    /// Environment variables to forward (filtered by the client).
    pub env: Vec<(String, String)>,
}
