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
    }

    #[test]
    fn canonicalize_nonexistent_passes_through_unchanged() {
        let path =
            std::env::temp_dir().join(format!("hallouminate-nonexistent-{}", std::process::id()));
        let resolved = canonicalize_or_passthrough(&path);
        assert_eq!(resolved.as_path(), path.as_path());
    }
}
