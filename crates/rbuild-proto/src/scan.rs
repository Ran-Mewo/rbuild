//! Filesystem scanning: turning a directory tree into a content-addressed
//! manifest both sides can diff.
//!
//! The client scans with ignore rules applied (`.gitignore` + `.rbuildignore`)
//! so build outputs and VCS metadata never sync. The daemon scans its mirror
//! without ignore rules — it must see exactly what it stored so deletions are
//! detected. Both produce the same [`ManifestEntry`] shape keyed by relative,
//! forward-slashed path.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use crate::hash::Hash;
use crate::proto::ManifestEntry;

/// `.rbuild` is rbuild's own metadata and must never be synced.
const ALWAYS_IGNORE: &[&str] = &[".rbuild", ".git"];

/// A manifest as a path→entry map for cheap diffing. Paths use `/` on every
/// platform so a Windows client and Linux daemon agree.
pub type Manifest = BTreeMap<String, ManifestEntry>;

#[cfg(unix)]
fn file_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode()
}

#[cfg(not(unix))]
fn file_mode(_meta: &std::fs::Metadata) -> u32 {
    // Windows has no POSIX mode; the daemon applies a sane default on write.
    0o644
}

fn rel_path(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

/// Scans `root` applying gitignore-style rules. Used by the client.
pub fn scan_with_ignores(root: &Path) -> io::Result<Manifest> {
    let mut manifest = Manifest::new();
    let walker = ignore::WalkBuilder::new(root)
        .standard_filters(true)
        // Apply .gitignore even when the project isn't a git checkout — rbuild
        // projects often aren't, and build outputs must still be excluded.
        .require_git(false)
        .add_custom_ignore_filename(".rbuildignore")
        .hidden(false)
        .build();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if path == root {
            continue;
        }
        if let Some(first) = rel_path(root, path).and_then(|r| r.split('/').next().map(str::to_string)) {
            if ALWAYS_IGNORE.contains(&first.as_str()) {
                continue;
            }
        }
        let meta = match entry.metadata() {
            Ok(m) if m.is_file() => m,
            _ => continue,
        };
        if let Some(rel) = rel_path(root, path) {
            let data = std::fs::read(path)?;
            manifest.insert(
                rel.clone(),
                ManifestEntry {
                    path: rel,
                    hash: Hash::from_blake3(blake3::hash(&data)),
                    len: meta.len(),
                    mode: file_mode(&meta),
                },
            );
        }
    }
    Ok(manifest)
}

/// Scans `root` with no ignore rules. Used by the daemon on its mirror.
pub fn scan_raw(root: &Path) -> io::Result<Manifest> {
    let mut manifest = Manifest::new();
    if !root.exists() {
        return Ok(manifest);
    }
    scan_raw_inner(root, root, &mut manifest)?;
    Ok(manifest)
}

fn scan_raw_inner(root: &Path, dir: &Path, manifest: &mut Manifest) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let meta = entry.metadata()?;
        if meta.is_dir() {
            scan_raw_inner(root, &path, manifest)?;
        } else if meta.is_file() {
            if let Some(rel) = rel_path(root, &path) {
                let data = std::fs::read(&path)?;
                manifest.insert(
                    rel.clone(),
                    ManifestEntry {
                        path: rel,
                        hash: Hash::from_blake3(blake3::hash(&data)),
                        len: meta.len(),
                        mode: file_mode(&meta),
                    },
                );
            }
        }
    }
    Ok(())
}

/// Convenience: the content hash of a byte buffer, matching how manifest
/// entries are hashed.
pub fn content_hash(data: &[u8]) -> Hash {
    Hash::from_blake3(blake3::hash(data))
}

/// Given the local manifest and the remote's, returns `(to_send, to_delete)`:
/// paths whose content differs or is missing remotely, and paths present
/// remotely but gone locally.
pub fn diff(local: &Manifest, remote: &Manifest) -> (Vec<String>, Vec<String>) {
    let mut to_send = Vec::new();
    for (path, entry) in local {
        match remote.get(path) {
            Some(r) if r.hash == entry.hash => {}
            _ => to_send.push(path.clone()),
        }
    }
    let to_delete = remote
        .keys()
        .filter(|p| !local.contains_key(*p))
        .cloned()
        .collect();
    (to_send, to_delete)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_detects_changes_and_deletions() {
        let mut local = Manifest::new();
        local.insert(
            "a.txt".into(),
            ManifestEntry { path: "a.txt".into(), hash: Hash::of(b"a"), len: 1, mode: 0o644 },
        );
        local.insert(
            "b.txt".into(),
            ManifestEntry { path: "b.txt".into(), hash: Hash::of(b"b2"), len: 2, mode: 0o644 },
        );

        let mut remote = Manifest::new();
        remote.insert(
            "b.txt".into(),
            ManifestEntry { path: "b.txt".into(), hash: Hash::of(b"b"), len: 1, mode: 0o644 },
        );
        remote.insert(
            "gone.txt".into(),
            ManifestEntry { path: "gone.txt".into(), hash: Hash::of(b"x"), len: 1, mode: 0o644 },
        );

        let (mut send, del) = diff(&local, &remote);
        send.sort();
        assert_eq!(send, vec!["a.txt".to_string(), "b.txt".to_string()]);
        assert_eq!(del, vec!["gone.txt".to_string()]);
    }
}
