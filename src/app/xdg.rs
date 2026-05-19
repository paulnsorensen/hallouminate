//! XDG-style path resolution.
//!
//! Honors `$XDG_*_HOME` when set and non-empty, otherwise falls back to a
//! tilde-prefixed default and joins the requested subpath. One helper so
//! each XDG namespace (config, state, cache, data) doesn't grow its own
//! copy of the same env-var-with-tilde-fallback pattern.

use std::path::PathBuf;

/// Resolve an XDG-style path. Reads `env_var` from the process environment;
/// if missing or empty, falls back to `fallback_base` (tilde-expanded) and
/// joins each segment of `subpath` underneath.
pub fn xdg_path(env_var: &str, fallback_base: &str, subpath: &[&str]) -> PathBuf {
    xdg_path_from(std::env::var_os(env_var).as_deref(), fallback_base, subpath)
}

/// Pure resolver split out so tests can exercise both branches without
/// mutating process env (unsafe on edition 2024) or relying on the
/// developer's local shell environment.
pub fn xdg_path_from(
    env_value: Option<&std::ffi::OsStr>,
    fallback_base: &str,
    subpath: &[&str],
) -> PathBuf {
    let base = env_value
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(shellexpand::tilde(fallback_base).into_owned()));
    subpath.iter().fold(base, |p, seg| p.join(seg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn honors_env_var_when_set() {
        let p = xdg_path_from(
            Some(std::ffi::OsStr::new("/var/xdg")),
            "~/.local/state",
            &["hallouminate", "x.log"],
        );
        assert_eq!(p, PathBuf::from("/var/xdg/hallouminate/x.log"));
    }

    #[test]
    fn empty_env_var_treated_as_unset() {
        let p = xdg_path_from(
            Some(std::ffi::OsStr::new("")),
            "~/.local/state",
            &["hallouminate"],
        );
        assert!(
            p.ends_with(".local/state/hallouminate"),
            "unexpected fallback: {p:?}"
        );
        assert!(p.is_absolute(), "tilde must expand: {p:?}");
    }

    #[test]
    fn unset_env_var_falls_back_to_tilde_expanded_base() {
        let p = xdg_path_from(None, "~/.config", &["hallouminate", "config.toml"]);
        assert!(
            p.ends_with(".config/hallouminate/config.toml"),
            "unexpected fallback: {p:?}"
        );
        assert!(p.is_absolute(), "tilde must expand: {p:?}");
    }

    #[test]
    fn empty_subpath_returns_base() {
        let p = xdg_path_from(Some(std::ffi::OsStr::new("/x")), "~/.local/state", &[]);
        assert_eq!(p, PathBuf::from("/x"));
    }
}
