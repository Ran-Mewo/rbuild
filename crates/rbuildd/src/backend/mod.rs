//! Build execution backends.
//!
//! A backend takes a [`BuildRequest`] and a workspace directory, runs the
//! command in isolation, streams its output back through a sink, and reports
//! the exit code. Selection is by [`Target`]: Linux builds use a Linux
//! container; Windows builds run through Wine in a Linux container (added in a
//! later phase). Keeping this behind a trait means a future native-Windows or
//! KVM backend drops in without touching the serve loop.

use anyhow::Result;
use rbuild_proto::proto::{BuildRequest, OutputFd, Target};

pub mod docker;

/// Receives build output as it is produced. Implementations forward each line
/// to the client so the user sees a live build, exactly as if it were local.
#[async_trait::async_trait]
pub trait OutputSink: Send {
    async fn line(&mut self, fd: OutputFd, text: &str) -> Result<()>;
    async fn started(&mut self, container: &str) -> Result<()>;
}

#[async_trait::async_trait]
pub trait BuildBackend: Send + Sync {
    /// Runs the build to completion, returning the process exit code.
    /// `ws_volume` is the Docker volume holding the workspace mirror (mounted
    /// at the build's working dir); `cache_volume` is a persistent volume
    /// mounted as the build user's HOME, so toolchain caches and the Wine
    /// prefix survive across builds without touching any host filesystem.
    async fn run(
        &self,
        req: &BuildRequest,
        ws_volume: &str,
        cache_volume: &str,
        sink: &mut dyn OutputSink,
    ) -> Result<i32>;
}

/// Picks a backend for the requested target.
pub fn for_target(target: Target, linux_image: &str, wine_image: &str) -> Box<dyn BuildBackend> {
    match target {
        Target::Linux => Box::new(docker::DockerBackend::linux(linux_image)),
        Target::Windows => Box::new(docker::DockerBackend::wine(wine_image)),
    }
}
