//! Configuration.
//!
//! rbuild is configured once, globally: a remote to build on, one or more
//! *code roots* (directories like `~/Code` that hold all your projects), and
//! shared build settings. There is no per-project file — pointing rbuild at a
//! code root makes every project beneath it work, and your code directories
//! stay clean (shims and state live under rbuild's own config dir).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::proto::Target;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalConfig {
    pub remote: RemoteConfig,

    /// Registered code roots. A directory at or below one is "inside rbuild" —
    /// its build commands are intercepted and its tree syncs to the remote.
    #[serde(default)]
    pub roots: Vec<Root>,

    #[serde(default)]
    pub build: BuildSettings,
}

/// A code root: a local directory and the name of the workspace it maps to on
/// the remote. By default the workspace name is derived from the absolute path,
/// so each machine's roots are isolated. Giving two machines' roots the *same*
/// `name` makes them share one remote workspace (and merge).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Root {
    pub path: PathBuf,
    /// Shared workspace name. Two roots with the same name (on any machine)
    /// map to the same remote workspace.
    pub name: String,
}

impl Root {
    /// The remote workspace id this root maps to — a hash of the shared name, so
    /// different local paths with the same name collide intentionally.
    pub fn workspace_id(&self) -> String {
        workspace_id_for_name(&self.name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    /// SSH destination, e.g. `build-host` or `user@1.2.3.4`. Resolved through
    /// the user's normal SSH config — we never manage ports or credentials.
    pub host: String,
    /// Optional explicit identity file, otherwise SSH picks per its own config.
    #[serde(default)]
    pub identity_file: Option<PathBuf>,
    /// How to invoke Docker on the remote: `docker` or `sudo docker`. Detected
    /// on first connect (some hosts need sudo for the Docker socket) and cached
    /// here. Empty means "not yet detected".
    #[serde(default)]
    pub docker: String,
}

/// Build behavior shared across every code root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildSettings {
    /// Build commands to intercept by argv[0]. Anything not listed is never
    /// shimmed, so `grep`, `cd`, and the like always run locally.
    #[serde(default = "default_commands")]
    pub commands: Vec<String>,
    /// Container image for Linux-target builds.
    #[serde(default = "default_linux_image")]
    pub linux_image: String,
    /// Container image for Windows-target builds (Wine).
    #[serde(default = "default_wine_image")]
    pub wine_image: String,
    /// Globs (relative to the directory a build runs in) to sync back as
    /// artifacts. Only these are mirrored remote→local.
    #[serde(default = "default_artifacts")]
    pub artifacts: Vec<String>,
}

impl Default for BuildSettings {
    fn default() -> Self {
        BuildSettings {
            commands: default_commands(),
            linux_image: default_linux_image(),
            wine_image: default_wine_image(),
            artifacts: default_artifacts(),
        }
    }
}

fn default_commands() -> Vec<String> {
    ["cargo", "cmake", "make", "gradle", "mvn", "pip", "npm", "go", "msbuild"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn default_linux_image() -> String {
    "rbuild/linux:latest".to_string()
}

fn default_wine_image() -> String {
    "rbuild/wine:latest".to_string()
}

fn default_artifacts() -> Vec<String> {
    vec!["target/**".to_string()]
}

/// A build location: which code root, the workspace id it maps to on the
/// remote, and the directory the command runs in relative to that root.
pub struct Location {
    pub root: PathBuf,
    pub workspace_id: String,
    pub rel_cwd: String,
}

impl GlobalConfig {
    pub fn config_dir() -> Result<PathBuf> {
        let dirs = directories::ProjectDirs::from("", "", "rbuild")
            .context("could not determine config directory")?;
        Ok(dirs.config_dir().to_path_buf())
    }

    pub fn path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("config.toml"))
    }

    /// The single directory holding the command shims, on PATH only while a
    /// shell is inside a code root. Lives under the config dir so code roots
    /// stay clean.
    pub fn shim_dir() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("shims"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading global config at {}", path.display()))?;
        toml::from_str(&text).context("parsing global config")
    }

    /// Loads config if present, else a minimal default (used by the hook/shim
    /// fast paths where absence simply means "not set up").
    pub fn load_or_default() -> Option<Self> {
        Self::load().ok()
    }

    pub fn save(&self) -> Result<()> {
        let dir = Self::config_dir()?;
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("config.toml"), toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Adds a code root (idempotently by path) with the given workspace name.
    /// If a root for the same path exists, its name is updated.
    pub fn add_root(&mut self, dir: &Path, name: String) {
        let dir = dir.to_path_buf();
        if let Some(existing) = self.roots.iter_mut().find(|r| r.path == dir) {
            existing.name = name;
        } else {
            self.roots.push(Root { path: dir, name });
        }
    }

    /// Returns the build location for `cwd` if it lies within a registered code
    /// root. When roots nest, the deepest (longest) match wins.
    pub fn locate(&self, cwd: &Path) -> Option<Location> {
        let root = self
            .roots
            .iter()
            .filter(|r| cwd.starts_with(&r.path))
            .max_by_key(|r| r.path.as_os_str().len())?;
        let rel_cwd = cwd
            .strip_prefix(&root.path)
            .ok()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        Some(Location {
            workspace_id: root.workspace_id(),
            root: root.path.clone(),
            rel_cwd,
        })
    }

    /// Image to use for a target.
    pub fn image_for(&self, target: Target) -> &str {
        match target {
            Target::Linux => &self.build.linux_image,
            Target::Windows => &self.build.wine_image,
        }
    }
}

/// Default workspace name for a freshly added root: the directory's own name,
/// Default workspace name for a freshly added root: the directory's own name.
/// Same-named folders on different machines therefore share a workspace (e.g.
/// `~/Code` and `D:/Code` both → `Code`); differently-named folders stay
/// separate. Use `--as` to override when two same-named local dirs should not
/// merge, or to pick a custom shared name.
pub fn default_workspace_name(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "root".to_string())
}

/// Stable workspace id from a (possibly shared) workspace name. Two machines
/// that use the same name get the same id, hence the same remote workspace.
pub fn workspace_id_for_name(name: &str) -> String {
    // Keep a readable prefix, disambiguated by a hash of the full name so that
    // odd characters or long names still produce a safe single-segment id.
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let prefix: String = safe.chars().take(24).collect();
    let digest = crate::hash::Hash::of(name.as_bytes()).to_hex();
    format!("{prefix}-{}", &digest[..12])
}
