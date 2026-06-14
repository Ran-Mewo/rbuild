//! Three-way merge for two-way sync.
//!
//! Each client keeps an *ancestor* snapshot: the manifest as it stood after the
//! last successful sync. Comparing local-vs-ancestor and remote-vs-ancestor
//! tells us who changed what since the machines were last in agreement, which
//! is what lets two machines share a workspace without one clobbering the other.
//!
//! The guiding rule is **never lose data**: a file changed on both sides in
//! different ways becomes a conflict, where the local copy is kept and the
//! remote copy is written beside it as a sidecar — nothing is silently
//! overwritten or deleted.

use std::collections::BTreeSet;

use crate::scan::Manifest;

/// What the client should do to reconcile local, ancestor, and remote.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MergePlan {
    /// Push these local files to the remote (new or changed locally).
    pub push: Vec<String>,
    /// Pull these remote files (new or changed remotely).
    pub pull: Vec<String>,
    /// Delete these on the remote (deleted locally, unchanged remotely).
    pub delete_remote: Vec<String>,
    /// Delete these locally (deleted remotely, unchanged locally).
    pub delete_local: Vec<String>,
    /// Changed differently on both sides. Resolved by keeping local and pulling
    /// the remote copy into a sidecar; never auto-overwritten.
    pub conflicts: Vec<String>,
}

impl MergePlan {
    pub fn is_empty(&self) -> bool {
        self.push.is_empty()
            && self.pull.is_empty()
            && self.delete_remote.is_empty()
            && self.delete_local.is_empty()
            && self.conflicts.is_empty()
    }
}

/// Per-file change relative to the ancestor.
#[derive(PartialEq, Eq)]
enum Change {
    Absent,
    Same,     // present, identical to ancestor
    Modified, // present, differs from ancestor
}

fn classify(side: Option<&crate::proto::ManifestEntry>, ancestor: Option<&crate::proto::ManifestEntry>) -> Change {
    match (side, ancestor) {
        (None, _) => Change::Absent,
        (Some(s), Some(a)) if s.hash == a.hash => Change::Same,
        (Some(_), None) => Change::Modified, // new since ancestor
        (Some(_), Some(_)) => Change::Modified,
    }
}

/// Computes the merge plan from the three manifests. `ancestor` is empty on the
/// first sync of a workspace, which makes every file on either side an addition
/// — so two populated trees union instead of one deleting the other.
pub fn plan(local: &Manifest, ancestor: &Manifest, remote: &Manifest) -> MergePlan {
    let mut out = MergePlan::default();

    // Union of all paths seen anywhere.
    let mut paths: BTreeSet<&String> = BTreeSet::new();
    paths.extend(local.keys());
    paths.extend(ancestor.keys());
    paths.extend(remote.keys());

    for path in paths {
        let l = local.get(path);
        let r = remote.get(path);
        let a = ancestor.get(path);

        let lc = classify(l, a);
        let rc = classify(r, a);

        // If local and remote already agree on content, there's nothing to do
        // regardless of the ancestor (covers "both added the same file").
        if let (Some(lh), Some(rh)) = (l, r) {
            if lh.hash == rh.hash {
                continue;
            }
        }

        match (lc, rc) {
            // Unchanged on a side → take the other side's change.
            (Change::Same, Change::Modified) => out.pull.push(path.clone()),
            (Change::Modified, Change::Same) => out.push.push(path.clone()),

            // Present only one side, ancestor had it, other side deleted it.
            (Change::Same, Change::Absent) => out.delete_local.push(path.clone()),
            (Change::Absent, Change::Same) => out.delete_remote.push(path.clone()),

            // New on exactly one side (ancestor absent).
            (Change::Modified, Change::Absent) if a.is_none() => out.push.push(path.clone()),
            (Change::Absent, Change::Modified) if a.is_none() => out.pull.push(path.clone()),

            // Modified on one side, deleted on the other (ancestor existed):
            // a delete-vs-edit conflict. Keep the surviving content; the edited
            // side wins the file, the deleted side gets it (re)created.
            (Change::Modified, Change::Absent) => out.push.push(path.clone()),
            (Change::Absent, Change::Modified) => out.pull.push(path.clone()),

            // Both modified differently → conflict (hashes differ, checked above).
            (Change::Modified, Change::Modified) => out.conflicts.push(path.clone()),

            // Deleted on both, or unchanged on both → nothing to do.
            (Change::Absent, Change::Absent) | (Change::Same, Change::Same) => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::Hash;
    use crate::proto::ManifestEntry;

    fn m(pairs: &[(&str, &str)]) -> Manifest {
        pairs
            .iter()
            .map(|(p, c)| {
                (
                    p.to_string(),
                    ManifestEntry {
                        path: p.to_string(),
                        hash: Hash::of(c.as_bytes()),
                        len: c.len() as u64,
                        mode: 0o644,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn first_sync_unions_both_trees() {
        // No ancestor: local has a, remote has b → push a, pull b, delete nothing.
        let local = m(&[("a", "1")]);
        let remote = m(&[("b", "2")]);
        let p = plan(&local, &Manifest::new(), &remote);
        assert_eq!(p.push, vec!["a"]);
        assert_eq!(p.pull, vec!["b"]);
        assert!(p.delete_local.is_empty() && p.delete_remote.is_empty());
        assert!(p.conflicts.is_empty());
    }

    #[test]
    fn local_edit_pushes_remote_unchanged() {
        let ancestor = m(&[("a", "1")]);
        let local = m(&[("a", "2")]); // edited locally
        let remote = m(&[("a", "1")]); // unchanged
        let p = plan(&local, &ancestor, &remote);
        assert_eq!(p.push, vec!["a"]);
        assert!(p.pull.is_empty() && p.conflicts.is_empty());
    }

    #[test]
    fn remote_edit_pulls_local_unchanged() {
        let ancestor = m(&[("a", "1")]);
        let local = m(&[("a", "1")]);
        let remote = m(&[("a", "2")]);
        let p = plan(&local, &ancestor, &remote);
        assert_eq!(p.pull, vec!["a"]);
        assert!(p.push.is_empty());
    }

    #[test]
    fn delete_propagates_when_other_side_unchanged() {
        let ancestor = m(&[("a", "1"), ("b", "1")]);
        let local = m(&[("a", "1")]); // deleted b locally
        let remote = m(&[("a", "1"), ("b", "1")]);
        let p = plan(&local, &ancestor, &remote);
        assert_eq!(p.delete_remote, vec!["b"]);
        assert!(p.delete_local.is_empty());
    }

    #[test]
    fn concurrent_different_edits_conflict() {
        let ancestor = m(&[("a", "1")]);
        let local = m(&[("a", "2")]);
        let remote = m(&[("a", "3")]);
        let p = plan(&local, &ancestor, &remote);
        assert_eq!(p.conflicts, vec!["a"]);
        assert!(p.push.is_empty() && p.pull.is_empty());
    }

    #[test]
    fn identical_concurrent_edit_is_noop() {
        // Both machines made the same change → no conflict, nothing to transfer.
        let ancestor = m(&[("a", "1")]);
        let local = m(&[("a", "2")]);
        let remote = m(&[("a", "2")]);
        let p = plan(&local, &ancestor, &remote);
        assert!(p.is_empty());
    }
}
