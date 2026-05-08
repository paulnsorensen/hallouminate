use std::path::{Path, PathBuf};

use crate::domains::common::FileRef;

pub fn expand_tilde(path: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(path).into_owned())
}

pub fn canonicalize_or_passthrough(path: &Path) -> FileRef {
    match std::fs::canonicalize(path) {
        Ok(canonical) => FileRef::new(canonical),
        Err(_) => FileRef::new(path.to_path_buf()),
    }
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
}
