//! Daemon socket path resolution.

use std::ffi::{CStr, OsStr};
use std::io;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const SOCKET_NAME: &str = "daemon.sock";

/// The canonical and, during migration, legacy daemon socket candidates.
pub(crate) struct DaemonSocketPaths {
    canonical: PathBuf,
    legacy: Option<PathBuf>,
}

impl DaemonSocketPaths {
    pub(crate) fn canonical(&self) -> &Path {
        &self.canonical
    }
    pub(crate) fn legacy(&self) -> Option<&Path> {
        self.legacy.as_deref()
    }
}

/// Resolve the exact daemon socket override or canonical account-home path.
pub fn daemon_socket_path() -> io::Result<PathBuf> {
    Ok(daemon_socket_paths()?.canonical)
}

/// Resolve the explicit override or canonical/legacy connection candidates.
pub(crate) fn daemon_socket_paths() -> io::Result<DaemonSocketPaths> {
    let explicit = std::env::var_os("HALLOUMINATE_SOCKET");
    if let Some(explicit) = explicit.as_deref().filter(|value| !value.is_empty()) {
        return Ok(DaemonSocketPaths {
            canonical: PathBuf::from(explicit),
            legacy: None,
        });
    }

    let runtime = std::env::var_os("XDG_RUNTIME_DIR");
    daemon_socket_paths_from(
        None,
        runtime.as_deref(),
        rustix::process::geteuid().as_raw(),
        account_home_for_current_user()?,
    )
}

fn account_home_for_current_user() -> io::Result<PathBuf> {
    let uid = rustix::process::geteuid().as_raw();
    let mut passwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut buffer = vec![0_u8; 16 * 1024];
    let mut result = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            passwd.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return Err(io::Error::other(format!(
            "cannot resolve account home for effective uid {uid}; set HALLOUMINATE_SOCKET to an explicit socket path"
        )));
    }
    let home = unsafe { CStr::from_ptr((*result).pw_dir) };
    Ok(PathBuf::from(OsStr::from_bytes(home.to_bytes())))
}

fn daemon_socket_paths_from(
    explicit_socket: Option<&OsStr>,
    xdg_runtime_dir: Option<&OsStr>,
    euid: u32,
    account_home: PathBuf,
) -> io::Result<DaemonSocketPaths> {
    if let Some(explicit) = explicit_socket.filter(|value| !value.is_empty()) {
        return Ok(DaemonSocketPaths {
            canonical: PathBuf::from(explicit),
            legacy: None,
        });
    }
    let canonical = account_home
        .join(".cache")
        .join("hallouminate")
        .join(SOCKET_NAME);
    let legacy = legacy_socket_path(xdg_runtime_dir, euid);
    Ok(DaemonSocketPaths {
        legacy: (legacy != canonical).then_some(legacy),
        canonical,
    })
}

fn legacy_socket_path(xdg_runtime_dir: Option<&OsStr>, euid: u32) -> PathBuf {
    match xdg_runtime_dir.filter(|value| !value.is_empty()) {
        Some(runtime) => PathBuf::from(runtime)
            .join("hallouminate")
            .join(SOCKET_NAME),
        None => PathBuf::from(format!("/run/user/{euid}"))
            .join("hallouminate")
            .join(SOCKET_NAME),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_path_uses_account_home_not_xdg() {
        let paths = daemon_socket_paths_from(
            None,
            Some(OsStr::new("/run/user/1000")),
            1000,
            PathBuf::from("/accounts/ada"),
        )
        .expect("paths");
        assert_eq!(
            paths.canonical(),
            Path::new("/accounts/ada/.cache/hallouminate/daemon.sock")
        );
        assert_eq!(
            paths.legacy(),
            Some(Path::new("/run/user/1000/hallouminate/daemon.sock"))
        );
    }

    #[test]
    fn explicit_socket_skips_legacy_discovery() {
        let paths = daemon_socket_paths_from(
            Some(OsStr::new("/tmp/explicit.sock")),
            Some(OsStr::new("/run/user/1000")),
            1000,
            PathBuf::from("/accounts/ada"),
        )
        .expect("paths");
        assert_eq!(paths.canonical(), Path::new("/tmp/explicit.sock"));
        assert_eq!(paths.legacy(), None);
    }

    #[test]
    fn absent_xdg_uses_conventional_legacy_runtime_path() {
        let paths = daemon_socket_paths_from(None, None, 1000, PathBuf::from("/accounts/ada"))
            .expect("paths");
        assert_eq!(
            paths.legacy(),
            Some(Path::new("/run/user/1000/hallouminate/daemon.sock"))
        );
    }
}
