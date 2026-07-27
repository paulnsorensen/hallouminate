use std::path::{Path, PathBuf};

use super::FileRef;

pub fn expand_tilde(path: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(path).into_owned())
}

/// Resolve symlinks and collapse `..` via [`std::fs::canonicalize`], falling
/// back to the original path on failure (typically: path does not exist, is
/// on a non-traversable mount, or the process lacks permission).
///
/// The error is intentionally swallowed: callers (the walker, the indexer)
/// need a stable [`FileRef`] for paths that may legitimately fail to
/// canonicalize, and there is no recovery the caller could perform with the
/// `io::Error` here. The unit tests pin both branches: `canonicalize_existing_dir_resolves`
/// exercises the success path, `canonicalize_nonexistent_passes_through_unchanged`
/// exercises the fallback.
pub fn canonicalize_or_passthrough(path: &Path) -> FileRef {
    match std::fs::canonicalize(path) {
        Ok(canonical) => FileRef::new(canonical),
        Err(_) => FileRef::new(path.to_path_buf()),
    }
}

/// A root confirmed absent from the filesystem via `io::ErrorKind::NotFound`
/// -- the entire safety proof `LanceStore::delete_root` accepts. Constructible
/// only through `retired_roots`, so a caller cannot pass an unverified path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredRoot(PathBuf);

impl RetiredRoot {
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

/// Roots confirmed absent via `NotFound`. Any other stat error (permission
/// denied, I/O error, stale/degraded mount) is treated as "cannot determine"
/// and is NOT retired -- fail closed, since the deletion this gates is
/// irreversible and machine-wide.
pub fn retired_roots(known: &[PathBuf]) -> Vec<RetiredRoot> {
    known
        .iter()
        .filter(|root| match std::fs::symlink_metadata(root) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(e) => {
                // Not retired -- fail closed -- but a root that is
                // permanently unstat-able (bad permissions, a persistently
                // stale mount) would otherwise never be collected AND never
                // be reported, silently under-delivering GC forever.
                tracing::warn!(
                    target: "hallouminate::lance",
                    root = %root.display(),
                    error = %e,
                    "root's filesystem status could not be determined; not retiring it",
                );
                false
            }
            Ok(_) => false,
        })
        .cloned()
        .map(RetiredRoot)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tilde_resolves_home() {
        let expanded = expand_tilde("~/foo");
        let base_dirs = directories::BaseDirs::new().expect("home directory not available");
        let home = base_dirs.home_dir();
        assert!(
            !expanded.to_string_lossy().starts_with('~'),
            "tilde should be replaced: {}",
            expanded.display()
        );
        assert!(
            expanded.starts_with(home),
            "{} should start with home ({})",
            expanded.display(),
            home.display()
        );
        assert!(
            expanded.ends_with("foo"),
            "{} should end with foo",
            expanded.display()
        );
    }

    #[test]
    fn expand_tilde_leaves_absolute_path_unchanged() {
        let expanded = expand_tilde("/etc/hosts");
        assert_eq!(expanded, PathBuf::from("/etc/hosts"));
    }

    #[test]
    fn expand_tilde_leaves_relative_path_unchanged() {
        let expanded = expand_tilde("foo/bar");
        assert_eq!(expanded, PathBuf::from("foo/bar"));
    }

    #[test]
    fn canonicalize_existing_dir_resolves() {
        let tmp = std::env::temp_dir();
        let resolved = canonicalize_or_passthrough(&tmp);
        let expected = std::fs::canonicalize(&tmp).expect("temp dir canonicalizes");
        assert_eq!(resolved.as_path(), expected.as_path());
        assert!(
            resolved.as_path().is_absolute(),
            "canonical path must be absolute: {}",
            resolved.as_path().display()
        );
    }

    #[cfg(unix)]
    #[test]
    fn canonicalize_resolves_symlink() {
        use std::os::unix::fs::symlink;
        let temp = std::env::temp_dir();
        let pid = std::process::id();
        let target = temp.join(format!("hallouminate-target-{}", pid));
        let link = temp.join(format!("hallouminate-link-{}", pid));
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir(&target);
        std::fs::create_dir(&target).expect("create target dir");
        symlink(&target, &link).expect("create symlink");

        let resolved = canonicalize_or_passthrough(&link);
        let expected = std::fs::canonicalize(&target).expect("canonicalize target");
        assert_eq!(resolved.as_path(), expected.as_path());
        assert_ne!(
            resolved.as_path(),
            link.as_path(),
            "symlink path should be resolved away"
        );

        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir(&target);
    }

    #[test]
    fn canonicalize_nonexistent_passes_through_unchanged() {
        let path =
            std::env::temp_dir().join(format!("hallouminate-nonexistent-{}", std::process::id()));
        let resolved = canonicalize_or_passthrough(&path);
        assert_eq!(resolved.as_path(), path.as_path());
    }

    #[test]
    fn retired_roots_returns_only_absent_paths() {
        let existing = tempfile::tempdir().expect("tempdir");
        let absent = existing.path().join("gone");
        let known = vec![existing.path().to_path_buf(), absent.clone()];
        assert_eq!(retired_roots(&known), vec![RetiredRoot(absent)]);
    }

    #[test]
    fn retired_roots_returns_empty_when_all_paths_exist() {
        let a = tempfile::tempdir().expect("tempdir");
        let b = tempfile::tempdir().expect("tempdir");
        let known = vec![a.path().to_path_buf(), b.path().to_path_buf()];
        assert!(retired_roots(&known).is_empty());
    }

    // Permission-denied stat errors aren't exercised here: CI (and some
    // local dev setups) may run as root, which bypasses permission checks
    // entirely and would make this test flaky rather than meaningful.

    #[cfg(unix)]
    #[test]
    fn retired_roots_does_not_retire_a_root_reachable_only_via_symlink() {
        use std::os::unix::fs::symlink;
        let real_target = tempfile::tempdir().expect("real target tempdir");
        let link_parent = tempfile::tempdir().expect("link parent tempdir");
        let link = link_parent.path().join("link-to-target");
        symlink(real_target.path(), &link).expect("create symlink");

        // The discriminating case: the link itself still exists, but its
        // target has been removed (a "dangling" symlink). `symlink_metadata`
        // (lstat) checks the link's own inode and never follows it, so this
        // must NOT be retired. Under `metadata` (stat, which follows), this
        // same setup would resolve to the missing target and report
        // `NotFound` -- incorrectly retiring it. This is the one assertion
        // that actually depends on `retired_roots` using `symlink_metadata`
        // rather than `metadata`.
        drop(real_target); // removes the target directory, leaving `link` dangling
        assert!(
            retired_roots(std::slice::from_ref(&link)).is_empty(),
            "a dangling symlink (link present, target gone) must not be retired -- \
             symlink_metadata checks the link itself, not what it points to"
        );

        // Documented limitation: roots are stored in canonical form, so once
        // the symlink is *also* removed, the stored (canonical) path was
        // never the link -- deleting the link changes nothing about whether
        // the canonical root is retired. This is the fail-safe
        // under-collection the spec's Known Limitation section describes:
        // a symlink can be deleted without the GC pass ever noticing,
        // because production never stores the symlink path as a root.
        std::fs::remove_file(&link).expect("remove symlink");
        assert_eq!(
            retired_roots(std::slice::from_ref(&link)).len(),
            1,
            "once the link itself is gone, its own path (not a canonical root anyone stores) is a plain absent path"
        );
    }
}
