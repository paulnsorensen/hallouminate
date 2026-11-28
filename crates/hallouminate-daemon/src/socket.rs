//! Daemon socket path resolution.

use std::ffi::{CStr, OsStr};
use std::io;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use hallouminate_config::channel::{BUILD_CHANNEL, BuildChannel};

const SOCKET_NAME: &str = "daemon.sock";
const SOCKET_ENV: &str = "HALLOUMINATE_SOCKET";

/// Bytes in `sockaddr_un.sun_path`, including the trailing NUL: 108 on
/// Linux and 104 on macOS.
pub(crate) const SUN_PATH_CAPACITY: usize =
    std::mem::size_of::<libc::sockaddr_un>() - std::mem::offset_of!(libc::sockaddr_un, sun_path);

#[derive(Debug, Clone, Copy)]
pub(crate) enum SocketSource {
    Override,
    ExplicitFlag,
    Default,
}

/// Resolves the exact daemon socket override or canonical account-home path.
///
/// The canonical path is `<account-home>/.cache/hallouminate/daemon.sock` for a
/// release build and `<account-home>/.cache/hallouminate-dev/daemon.sock` for
/// any other build.
///
/// # Errors
///
/// Returns an error when `HALLOUMINATE_SOCKET` is unset or empty and the OS
/// has no account record for the effective user, or when the resolved path
/// does not fit in a Unix socket address.
pub fn daemon_socket_path() -> io::Result<PathBuf> {
    resolve_socket_path(
        std::env::var_os(SOCKET_ENV).as_deref(),
        account_home_for_current_user,
        BUILD_CHANNEL,
    )
}

fn resolve_socket_path(
    explicit: Option<&OsStr>,
    account_home: impl FnOnce() -> io::Result<PathBuf>,
    channel: BuildChannel,
) -> io::Result<PathBuf> {
    if let Some(explicit) = explicit.filter(|value| !value.is_empty()) {
        return checked_socket_path(PathBuf::from(explicit), SocketSource::Override);
    }
    checked_socket_path(
        canonical_socket_path(account_home()?, channel),
        SocketSource::Default,
    )
}

pub(crate) fn checked_socket_path(path: PathBuf, source: SocketSource) -> io::Result<PathBuf> {
    let length = path.as_os_str().as_bytes().len();
    if length < SUN_PATH_CAPACITY {
        return Ok(path);
    }
    let max = SUN_PATH_CAPACITY - 1;
    let (subject, hint) = match source {
        SocketSource::Override => (
            format!("{SOCKET_ENV} path"),
            format!("set {SOCKET_ENV} to a shorter path"),
        ),
        SocketSource::ExplicitFlag => (
            "--socket path".to_string(),
            "pass a shorter path with --socket".to_string(),
        ),
        SocketSource::Default => (
            "default daemon socket path".to_string(),
            format!("set {SOCKET_ENV} to a shorter path"),
        ),
    };
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "{subject} {} is {length} bytes; a Unix socket path must be at most {max} bytes \
             ({SUN_PATH_CAPACITY} with the trailing NUL); {hint}",
            path.display()
        ),
    ))
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

fn canonical_socket_path(account_home: PathBuf, channel: BuildChannel) -> PathBuf {
    account_home
        .join(".cache")
        .join(channel.app_dir())
        .join(SOCKET_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHANNELS: [BuildChannel; 2] = [BuildChannel::Release, BuildChannel::Dev];

    fn ada_home() -> io::Result<PathBuf> {
        Ok(PathBuf::from("/accounts/ada"))
    }

    #[test]
    fn release_canonical_path_keeps_the_installed_location() {
        assert_eq!(
            canonical_socket_path(PathBuf::from("/accounts/ada"), BuildChannel::Release),
            PathBuf::from("/accounts/ada/.cache/hallouminate/daemon.sock")
        );
    }

    #[test]
    fn dev_canonical_path_differs_from_release() {
        assert_eq!(
            canonical_socket_path(PathBuf::from("/accounts/ada"), BuildChannel::Dev),
            PathBuf::from("/accounts/ada/.cache/hallouminate-dev/daemon.sock")
        );
    }

    #[test]
    fn compiled_default_socket_follows_the_build_channel() {
        let resolved = resolve_socket_path(None, ada_home, BUILD_CHANNEL).expect("default");
        let release = canonical_socket_path(PathBuf::from("/accounts/ada"), BuildChannel::Release);
        match BUILD_CHANNEL {
            BuildChannel::Release => assert_eq!(resolved, release),
            BuildChannel::Dev => assert_ne!(resolved, release),
        }
    }

    #[test]
    fn explicit_override_wins_in_both_channels() {
        for channel in CHANNELS {
            let resolved =
                resolve_socket_path(Some(OsStr::new("/tmp/h/custom.sock")), ada_home, channel)
                    .expect("override");
            assert_eq!(resolved, PathBuf::from("/tmp/h/custom.sock"));
        }
    }

    #[test]
    fn empty_override_falls_back_to_the_channel_default() {
        for channel in CHANNELS {
            let resolved =
                resolve_socket_path(Some(OsStr::new("")), ada_home, channel).expect("default");
            assert_eq!(
                resolved,
                canonical_socket_path(PathBuf::from("/accounts/ada"), channel)
            );
        }
    }

    #[test]
    fn sun_path_capacity_matches_the_platform_limit() {
        #[cfg(target_os = "linux")]
        assert_eq!(SUN_PATH_CAPACITY, 108);
        #[cfg(target_os = "macos")]
        assert_eq!(SUN_PATH_CAPACITY, 104);
    }

    #[test]
    fn longest_fitting_override_is_accepted() {
        let path = format!("/{}", "s".repeat(SUN_PATH_CAPACITY - 2));
        assert_eq!(path.len(), SUN_PATH_CAPACITY - 1);
        let resolved =
            resolve_socket_path(Some(OsStr::new(&path)), ada_home, BUILD_CHANNEL).expect("fits");
        assert_eq!(resolved, PathBuf::from(path));
    }

    #[test]
    fn over_long_override_names_the_variable_and_the_limit() {
        let path = format!("/{}", "s".repeat(SUN_PATH_CAPACITY - 1));
        let err = resolve_socket_path(Some(OsStr::new(&path)), ada_home, BUILD_CHANNEL)
            .expect_err("too long");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let msg = err.to_string();
        assert!(msg.starts_with("HALLOUMINATE_SOCKET path"), "got: {msg}");
        assert!(
            msg.contains(&format!("at most {} bytes", SUN_PATH_CAPACITY - 1)),
            "got: {msg}"
        );
        assert!(
            msg.contains(&format!("{SUN_PATH_CAPACITY} with the trailing NUL")),
            "got: {msg}"
        );
    }

    #[test]
    fn over_long_default_path_points_at_the_override() {
        let home = || Ok(PathBuf::from(format!("/{}", "h".repeat(SUN_PATH_CAPACITY))));
        let err = resolve_socket_path(None, home, BUILD_CHANNEL).expect_err("too long");
        let msg = err.to_string();
        assert!(msg.starts_with("default daemon socket path"), "got: {msg}");
        assert!(msg.contains("set HALLOUMINATE_SOCKET"), "got: {msg}");
    }
}
