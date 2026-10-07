//! Daemon socket path resolution.

#[cfg(unix)]
use std::ffi::CStr;
#[cfg(unix)]
use std::ffi::OsStr;
use std::io;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

const SOCKET_NAME: &str = "daemon.sock";

/// Resolves the exact daemon socket override or canonical account-home path.
///
/// # Errors
///
/// Returns an error when `HALLOUMINATE_SOCKET` is unset or empty and the OS
/// has no account record for the effective user.
pub fn daemon_socket_path() -> io::Result<PathBuf> {
    let explicit = std::env::var_os("HALLOUMINATE_SOCKET");
    if let Some(explicit) = explicit.as_deref().filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(explicit));
    }
    #[cfg(unix)]
    {
        Ok(canonical_socket_path(account_home_for_current_user()?))
    }
    #[cfg(windows)]
    {
        Ok(windows_socket_path(crate::windows_security::account_home()?))
    }
}

#[cfg(unix)]
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

#[cfg(unix)]
fn canonical_socket_path(account_home: PathBuf) -> PathBuf {
    account_home
        .join(".cache")
        .join("hallouminate")
        .join(SOCKET_NAME)
}

#[cfg(windows)]
fn windows_socket_path(account_home: PathBuf) -> PathBuf {
    account_home
        .join("AppData")
        .join("Local")
        .join("hallouminate")
        .join(SOCKET_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn canonical_path_uses_account_home() {
        assert_eq!(
            canonical_socket_path(PathBuf::from("/accounts/ada")),
            PathBuf::from("/accounts/ada/.cache/hallouminate/daemon.sock")
        );
    }

    #[cfg(windows)]
    #[test]
    fn canonical_path_uses_windows_account_home() {
        assert_eq!(
            windows_socket_path(PathBuf::from(r"C:\Users\ada")),
            PathBuf::from(r"C:\Users\ada\AppData\Local\hallouminate\daemon.sock")
        );
    }
}
